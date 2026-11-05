# dyl_flutter example

Runnable Android host for the Dyl Player kernel: plays a video on a
`SurfaceView`, and overlays ASS subtitles and danmaku on top.

## 首次准备

```powershell
# 1. FFmpeg for Android（只需一次，~29 MB 静态库）
sh ../../scripts/build_ffmpeg_android.sh

# 2. libdyl.so → jniLibs（cargo 产物 + 拷贝一步到位）
sh ../../scripts/build_android_lib.sh
```

## 运行

直接跑：

```powershell
cd dart/dyl_flutter/example
flutter run
```

界面上有三个按钮：**Video / Subtitle / Danmaku**，点开就是系统文件选择器
（SAF），选完立刻生效。启动时**不加载也不播放任何文件** —— 视频一律由选择器
选。想跳过交互做自动化验证时，字幕/弹幕仍可以用 `--dart-define` 预置路径
（内核在 `idle` 拒 `load_subtitle` / `load_danmaku`，所以它们等首个
`file_loaded` 之后才挂上）：

```powershell
flutter run --dart-define=DYL_SUBTITLE=<001.ass> `
           --dart-define=DYL_DANMAKU=<danmaku.xml>
```

### 关于"选文件"这件事

`DylPlayer.loadFile` 走 `std::path::Path::exists` 预检，然后让 FFmpeg
按**普通文件路径**打开。所以 SAF 直接回传的 `content://` URI 两样都不满足。

插件的做法是：`pickFile` 走 SAF 拿到 URI，**把文件复制到 app 私有目录**
（`filesDir/picked/`），再把那个副本的真实路径回传给 Dart：

```dart
final f = await DylFilePicker.pickFile(mimeTypes: ['video/*']);
if (f != null) client.loadFile(f.path);   // f.path 是真实可读路径
```

几个取舍：

- **视频限定 `video/*`**，字幕/弹幕不限（`*/*`）。`.ass` / `.xml` 在 Android 的
  MIME 表里没有正经类型，限定了文件管理器就直接看不见它们；改由 Dart 侧按扩展名校验。
- **复制而不是持有 URI 授权**：SAF 给的是临时读权限，URI 随时可能失效，
  而这个路径要交给 FFmpeg 长时间打开。落在 `filesDir` 而不是 `cacheDir`，
  后者会被系统清掉。
- **刻意不加超时**：人在目录里翻找几分钟是常事，任何时限都会把正常操作判成失败。
  真出现失联（Dart 侧按钮卡死）时，再点一次会被新的一次选择顶掉
  （`PICK_SUPERSEDED`），所以不会卡死。

### 路径怎么选

不想用选择器时，`--dart-define` 只能传 app 私有目录里的绝对路径：

- **app 私有目录不需要任何权限**，最省事。用 `adb push` 放进去：
  ```powershell
  adb push test_video.mp4 /data/local/tmp/
  adb shell run-as dev.dylplayer.dyl_player_example mkdir -p files
  adb shell run-as dev.dylplayer.dyl_player_example cp /data/local/tmp/test_video.mp4 files/
  # 然后 --dart-define=DYL_SUBTITLE / DYL_DANMAKU 指到 files/ 下的副本；
  # 视频没有 dart-define 入口，只能在界面里选。
  ```
- `/sdcard/...` 需要用户授予存储权限。manifest 里已经声明了
  `READ_MEDIA_VIDEO` / `READ_MEDIA_AUDIO`，但**本 example 不主动弹权限框** ——
  授了权就能用。
- `content://` URI **不支持**：`load_file` 的本地路径预检走
  `std::path::Path::exists`，所以要的是复制出来的真实路径（上面那段）。
  http(s) 地址内核是收的（`capabilities.protocols`），但这个 example 的选择器
  只会给出本地文件。

## 界面

- **Video / Subtitle / Danmaku** — 分别选视频、字幕（`.ass/.ssa/.srt/.vtt/.sub`）、
  弹幕（`.xml/.json`）文件。后两个在选到视频之前是禁用的：内核的 state matrix
  在 `idle` / `loading` 拒 `load_subtitle` / `load_danmaku`
- 进度条拖动 = `seek`
- AppBar 上的 `Subs on/off` / `Danmaku on/off` 切换字幕 / 弹幕的显示。这两个
  **不用图标 + tooltip**：视频是 Flutter 视图之上的子 HWND，tooltip 只会出现在
  锚点正上或正下，往上出不了 Flutter 视图、往下正压在 surface 上，两头都看不见，
  所以状态一律写进按钮文字
- `Fill`（或 `F`）把视频铺满整窗，`Esc` 退出；字母键会被中文输入法接走，
  所以退出键是 `Esc`
- 状态栏显示当前的编解码信息与错误信息

## 排查

Rust 侧日志走 logcat：

```powershell
adb logcat -s dyl
```

Dart 侧日志（surface attach / detach 之类）：

```powershell
adb logcat | Select-String "DylVideoView|flutter"
```

关键日志点：

| 日志 | 含义 |
|---|---|
| `surface attached (W x H, nativeWindow=0x...)` | Surface 已交给渲染线程 |
| `native reported surface destroyed, detaching` | 系统回收了 surface，正常路径 |
| `ExtRenderState initialized: WxH, format=..., present=...` | wgpu surface 创建成功（Vulkan/GLES 后端） |
| `no suitable GPU adapter` | 设备上没有可用的 Vulkan/GLES 驱动 |
| `load_system_font: no system font found` | Android 字体路径没命中，字幕/弹幕会是空的 |

如果画面全黑且没有 `surface attached`，先确认
`jniLibs/arm64-v8a/libdyl.so` 存在：

```powershell
Get-ChildItem android\app\src\main\jniLibs\arm64-v8a
```

## 已知限制

- 只打包 `arm64-v8a`（`abiFilters`）。32 位设备上装不上，这是有意的：
  没有对应的 `.so`，`DynamicLibrary.open` 必然失败。
- 真机 16KB page size（Android 15+）：NDK r28 默认按 16KB 对齐，理论上没问题，
  但未在 16KB 设备上验证过。
