plugins {
    id("com.android.application")
    id("kotlin-android")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    namespace = "dev.dylplayer.dyl_player_example"
    compileSdk = flutter.compileSdkVersion
    // Pinned to match dyl_flutter's plugin module and the aarch64-linux-android26
    // linker in .cargo/config.toml. libaaudio.so requires API 26+.
    ndkVersion = "28.2.13676358"

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = JavaVersion.VERSION_17.toString()
    }

    defaultConfig {
        applicationId = "dev.dylplayer.dyl_player_example"
        // 26, not flutter.minSdkVersion: libaaudio.so (AaudioOutput) only exists
        // from API 26. Must be >= the plugin's minSdk.
        minSdk = 26
        targetSdk = flutter.targetSdkVersion
        versionCode = flutter.versionCode
        versionName = flutter.versionName

        // libdyl.so is built for arm64-v8a only. Passing all ABIs would
        // make the APK claim support for 32-bit devices and then crash on
        // DynamicLibrary.open, since no 32-bit .so is shipped.
        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            // TODO: Add your own signing config for the release build.
            // Signing with the debug keys for now, so `flutter run --release` works.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
}

flutter {
    source = "../.."
}
