package dev.dylplayer.dyl_flutter

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.view.SurfaceHolder
import android.view.SurfaceView
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.embedding.engine.plugins.activity.ActivityAware
import io.flutter.embedding.engine.plugins.activity.ActivityPluginBinding
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.plugin.common.MethodChannel.MethodCallHandler
import io.flutter.plugin.common.MethodChannel.Result
import io.flutter.plugin.common.PluginRegistry.ActivityResultListener
import io.flutter.plugin.common.StandardMessageCodec
import io.flutter.plugin.platform.PlatformView
import io.flutter.plugin.platform.PlatformViewFactory
import java.io.File

/**
 * Dyl Flutter Plugin — Android SurfaceView 管理
 *
 * 注册 PlatformViewFactory 创建 SurfaceView，通过 MethodChannel 将
 * ANativeWindow* 指针传给 Dart 层，供 Rust FFI 的 dyl_surface_attach 使用。
 *
 * 另外实现了 `pickFile`：走 Storage Access Framework 选文件，并把结果**复制**到
 * app 私有目录再把真实路径回传。原因见 [onMethodCall] 里 pickFile 分支的注释。
 */
class DylFlutterPlugin :
    FlutterPlugin,
    MethodCallHandler,
    ActivityAware,
    ActivityResultListener {

    private lateinit var channel: MethodChannel
    private val surfaceViews = mutableMapOf<Int, DylPlatformSurfaceView>()

    private var activity: Activity? = null
    /** 等待中的 pickFile 回包；同一时刻只允许一个 pick 在飞。 */
    private var pendingPick: Result? = null

    override fun onAttachedToEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        channel = MethodChannel(binding.binaryMessenger, CHANNEL_NAME)
        channel.setMethodCallHandler(this)

        binding.platformViewRegistry.registerViewFactory(
            VIEW_TYPE,
            DylSurfaceViewFactory(this)
        )
    }

    override fun onDetachedFromEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        channel.setMethodCallHandler(null)
        surfaceViews.clear()
        activity = null
        // 引擎卸载时若有 pick 在飞，必须回包，否则 Dart 侧的 Future 永远不完成。
        pendingPick?.let {
            it.error("ENGINE_DETACHED", "engine detached during pickFile", null)
            pendingPick = null
        }
    }

    // ── ActivityAware ────────────────────────────────────────────
    //
    // startActivityForResult 需要 Activity，而 FlutterPlugin 本身没有。
    //
    // 注意用的是 registerForActivityResult 的替代路径：FlutterActivity 只是
    // Activity 的子类，不是 ComponentActivity，无法注册 ActivityResultCallback。
    //
    // 因此 onActivityResult 必须通过 ActivityPluginBinding 显式注册转发。
    // 漏掉这步的症状很隐蔽：pickFile 的 Result 永远收不到回包，
    // Dart 侧的 Future 永久挂起，UI 上表现为按钮一直是禁用状态。

    override fun onAttachedToActivity(binding: ActivityPluginBinding) {
        activity = binding.activity
        binding.addActivityResultListener(this)
    }

    override fun onDetachedFromActivityForConfigChanges() = detachActivity()

    override fun onReattachedToActivityForConfigChanges(binding: ActivityPluginBinding) {
        activity = binding.activity
        binding.addActivityResultListener(this)
    }

    override fun onDetachedFromActivity() = detachActivity()

    private fun detachActivity() {
        activity = null
        pendingPick?.let {
            it.error("NO_ACTIVITY", "activity detached during pickFile", null)
            pendingPick = null
        }
    }

    /**
     * 主动通知 Dart 某个 view 的 Surface 已销毁 / 重建。
     *
     * 必须有这条反向通道：SurfaceView 的 surface 会在息屏、Activity 重建、
     * 分屏切换时由 SurfaceFlinger 重建，这些时机**不会**产生
     * `AppLifecycleState` 变化，Dart 侧也就不会重新 attach —— 结果是回前台后
     * Rust 仍持有一个已失效的 ANativeWindow，表现为黑屏。
     *
     * 全部 post 到主线程：surfaceDestroyed 可能来自非 UI 线程，而
     * MethodChannel 不是线程安全的。
     */
    internal fun notifySurfaceEvent(method: String, viewId: Int) {
        if (!this::channel.isInitialized) return
        val messenger = channel
        android.os.Handler(android.os.Looper.getMainLooper()).post {
            runCatching { messenger.invokeMethod(method, mapOf("viewId" to viewId)) }
        }
    }

    override fun onMethodCall(call: MethodCall, result: Result) {
        when (call.method) {
            "getNativeWindow" -> {
                val viewId = call.argument<Int>("viewId")
                if (viewId == null) {
                    result.error("INVALID_ARGUMENT", "viewId is required", null)
                    return
                }
                val surfaceView = surfaceViews[viewId]
                if (surfaceView == null || !surfaceView.isSurfaceValid) {
                    result.error("SURFACE_NOT_READY", "Surface not available for view $viewId", null)
                    return
                }
                val nativeWindow = surfaceView.getNativeWindowPointer()
                val width = surfaceView.viewWidth
                val height = surfaceView.viewHeight
                result.success(mapOf(
                    "nativeWindow" to nativeWindow,
                    "width" to width,
                    "height" to height,
                ))
            }
            "destroyView" -> {
                val viewId = call.argument<Int>("viewId")
                if (viewId != null) {
                    surfaceViews.remove(viewId)
                }
                result.success(null)
            }
            // 归还一个 Dart 拿到但没交给 Rust 的 ANativeWindow 引用。
            "releaseNativeWindow" -> {
                val window = call.argument<Long>("window") ?: 0L
                if (window != 0L) {
                    DylPlatformSurfaceView.releaseNativeWindowPointer(window)
                }
                result.success(null)
            }
            // 通过 SAF 选择本地文件，回传一个**真实可读路径**。
            //
            // 为什么必须复制而不是直接回传 content:// URI：Rust 侧的
            // DylPlayer::load_file 会先做 `Path::exists()` 预检，再用
            // FFmpeg 按普通文件路径打开。content:// URI 两者都不满足。
            // 而且 SAF 授予的是"临时读权限"，URI 随时可能失效。
            "pickFile" -> {
                val act = activity
                if (act == null) {
                    result.error("NO_ACTIVITY", "no activity attached", null)
                    return
                }
                if (pendingPick != null) {
                    // 上一次选择不知怎么失联了（Activity 被系统干掉之类）。
                    // 直接放弃它 —— 报错总比让 Dart 侧的 Future 永久挂起、
                    // UI 上按钮永远点不动要好。用户重试一次就能恢复。
                    pendingPick?.error(
                        "PICK_SUPERSEDED",
                        "a new pickFile replaced an unfinished one",
                        null,
                    )
                    pendingPick = null
                }
                pendingPick = result

                val mimeTypes = call.argument<List<String>>("mimeTypes")
                val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    // 字幕/弹幕的扩展名（.ass/.ssa/.xml）在 Android 的 MIME 表里
                    // 基本没有对应项，所以调用方不传 mimeTypes 时一律 */*，
                    // 由 Dart 侧按扩展名过滤。视频则有正经的 MIME 类型，
                    // 限定后选择器可以直接走系统媒体索引。
                    if (mimeTypes.isNullOrEmpty()) {
                        type = "*/*"
                    } else {
                        type = mimeTypes.first()
                        if (mimeTypes.size > 1) {
                            putExtra(Intent.EXTRA_MIME_TYPES, mimeTypes.toTypedArray())
                        }
                    }
                    // 默认落点是 DocumentsUI 的"最近"，而它只收录**被别的应用通过
                    // SAF 打开过**的文档。刚装完、没装过其他文件管理器的机器上
                    // 这一页是空的，用户会以为选择器坏了。
                    // 所以尽量把它落在"下载"目录 —— 视频一般就躺在这儿。
                    // 注意：部分厂商 ROM 的 DocumentsUI 会忽略这个 extra，
                    // 忽略时静默退回默认落点，不会崩。
                    putExtra(
                        DocumentsContract.EXTRA_INITIAL_URI,
                        DocumentsContract.buildRootUri(
                            // DocumentsContract 没有公开这个 authority 常量
                            // （它在 ExternalStorageProvider 里，是平台内部约定）。
                            "com.android.externalstorage.documents",
                            "downloads",
                        ),
                    )
                }
                act.startActivityForResult(intent, PICK_FILE_REQUEST_CODE)
            }
            else -> result.notImplemented()
        }
    }

    internal fun registerSurfaceView(viewId: Int, view: DylPlatformSurfaceView) {
        surfaceViews[viewId] = view
    }

    // ── pickFile 结果处理 ────────────────────────────────────────

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?): Boolean {
        if (requestCode != PICK_FILE_REQUEST_CODE) {
            return false
        }
        val callback = pendingPick
        pendingPick = null
        if (callback == null) {
            return true
        }

        if (resultCode != Activity.RESULT_OK) {
            // 用户取消不算错误，返回 null 让调用方安静地什么都不做。
            callback.success(null)
            return true
        }

        val uri = data?.data
        if (uri == null) {
            callback.error("NO_URI", "pickFile returned no uri", null)
            return true
        }

        try {
            callback.success(copyToPrivateStorage(uri))
        } catch (e: Exception) {
            callback.error("COPY_FAILED", "failed to copy picked file: ${e.message}", null)
        }
        return true
    }

    /**
     * 把 SAF 返回的 content:// URI 复制到 app 私有目录，返回真实路径。
     *
     * 落在 filesDir 而不是 cacheDir：cacheDir 可能被系统清掉，而这个路径要
     * 交给 FFmpeg 持续打开读取。
     */
    private fun copyToPrivateStorage(uri: Uri): Map<String, Any?> {
        val ctx: Context = activity ?: throw IllegalStateException("no activity")
        val resolver = ctx.contentResolver

        val displayName = resolver.query(uri, null, null, null, null)?.use { cursor ->
            val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (idx >= 0 && cursor.moveToFirst()) cursor.getString(idx) else null
        } ?: "picked_${System.currentTimeMillis()}"

        val dir = File(ctx.filesDir, "picked").apply { mkdirs() }
        // 同名文件直接覆盖，避免 files/picked 无限增长。
        val dest = File(dir, displayName)

        resolver.openInputStream(uri).use { input ->
            requireNotNull(input) { "cannot open $uri" }
            dest.outputStream().use { output -> input.copyTo(output) }
        }

        return mapOf(
            "path" to dest.absolutePath,
            "name" to displayName,
            "size" to dest.length(),
        )
    }

    internal fun unregisterSurfaceView(viewId: Int) {
        surfaceViews.remove(viewId)
    }

    companion object {
        const val CHANNEL_NAME = "dev.dylplayer.dyl_flutter"
        const val VIEW_TYPE = "dev.dylplayer.dyl_flutter/surface_view"
        private const val PICK_FILE_REQUEST_CODE = 0x5241 // "RA"
    }
}

/**
 * PlatformViewFactory — 创建 SurfaceView PlatformView 实例
 */
class DylSurfaceViewFactory(
    private val plugin: DylFlutterPlugin
) : PlatformViewFactory(StandardMessageCodec.INSTANCE) {

    override fun create(context: Context, viewId: Int, args: Any?): PlatformView {
        val creationParams = args as? Map<*, *>
        val surfaceView = DylPlatformSurfaceView(context, viewId, plugin)
        plugin.registerSurfaceView(viewId, surfaceView)
        return surfaceView
    }
}

/**
 * PlatformView 包装器 — 管理 SurfaceView 和 SurfaceHolder 回调
 *
 * 通过 JNI `ANativeWindow_fromSurface` 获取原生窗口指针。
 * 注意：此指针在 Surface 有效期间有效，Surface 销毁后失效。
 */
class DylPlatformSurfaceView(
    context: Context,
    private val viewId: Int,
    private val plugin: DylFlutterPlugin
) : PlatformView, SurfaceHolder.Callback {

    private val surfaceView = SurfaceView(context)
    var isSurfaceValid = false
        private set
    var viewWidth = 0
        private set
    var viewHeight = 0
        private set

    private var surfaceHolder: SurfaceHolder? = null

    init {
        surfaceView.holder.addCallback(this)
    }

    override fun getView(): SurfaceView = surfaceView

    override fun dispose() {
        surfaceView.holder.removeCallback(this)
        plugin.unregisterSurfaceView(viewId)
    }

    /**
     * 获取 ANativeWindow* 指针（作为 long 值）
     *
     * 使用 JNI 调用 ANativeWindow_fromSurface(env, surface) 获取。
     * 此值在 Surface 有效期间有效。
     *
     * 注意：每次调用都会 +1 一个 native window 引用。调用方必须二选一 ——
     * 交给 Rust 的 `dyl_surface_attach`，并在 `dyl_surface_detach` 成功后
     * 调用 [releaseNativeWindowPointer] 归还；或拿到后直接归还。内核不会替你
     * release，引用泄漏会让 SurfaceView 的旧 Surface 和 buffer 队列一直存活，
     * 内存随前后台切换次数增长。
     */
    fun getNativeWindowPointer(): Long {
        val holder = surfaceHolder ?: return 0L
        return getNativeWindowFromSurface(holder.surface)
    }

    // ── SurfaceHolder.Callback ───────────────────────────────────
    //
    // Each callback also notifies Dart, because a surface can be recreated by
    // the system without any accompanying Flutter lifecycle change. Dart must
    // detach before the window goes away and re-attach on the new one, or the
    // renderer keeps presenting into a dead ANativeWindow (black screen).

    override fun surfaceCreated(holder: SurfaceHolder) {
        surfaceHolder = holder
        isSurfaceValid = true
        viewWidth = holder.surfaceFrame.width()
        viewHeight = holder.surfaceFrame.height()
        plugin.notifySurfaceEvent("onSurfaceCreated", viewId)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        val sizeChanged = width != viewWidth || height != viewHeight
        surfaceHolder = holder
        viewWidth = width
        viewHeight = height
        if (sizeChanged) {
            plugin.notifySurfaceEvent("onSurfaceResized", viewId)
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        isSurfaceValid = false
        surfaceHolder = null
        // Drop the ANativeWindow reference we handed to Rust. Dart is told to
        // call detachSurface() in response, and that call synchronously waits for
        // the render thread to release its own reference -- so by the time the
        // host tears the surface down nothing still points at it.
        plugin.notifySurfaceEvent("onSurfaceDestroyed", viewId)
    }

    // ── JNI ──────────────────────────────────────────────────────

    companion object {
        init {
            try {
                System.loadLibrary("dyl_flutter_jni")
            } catch (_: UnsatisfiedLinkError) {
                // JNI library not available — getNativeWindowFromSurface will return 0
            }
        }

        /**
         * JNI: 调用 ANativeWindow_fromSurface() 获取原生窗口指针。
         *
         * 如果 JNI 库未加载，返回 0。
         */
        @JvmStatic
        private external fun getNativeWindowFromSurface(surface: android.view.Surface): Long

        /**
         * JNI: 归还 getNativeWindowFromSurface() 拿到的引用。
         *
         * 只有在拿到指针却**没有**交给 `dyl_surface_attach` 时才需要调用 ——
         * 例如 client 已 dispose、attachSurface 抛异常、或 surface 在请求与回包
         * 之间被销毁。成功交给 Rust 后由 wgpu 在 Surface drop 时释放，重复释放
         * 会造成 use-after-free。
         */
        @JvmStatic
        private external fun releaseNativeWindow(window: Long)

        /**
         * 归还一个由 [getNativeWindowPointer] 取得、但没有交给 Rust 的
         * ANativeWindow 引用。
         *
         * 暴露成 companion 方法（而不是实例方法）是因为 Dart 可能在一个已经
         * dispose 的 view 上放弃交付，此时已经没有实例可调了。
         */
        @JvmStatic
        fun releaseNativeWindowPointer(window: Long) {
            if (window == 0L) return
            releaseNativeWindow(window)
        }
    }
}
