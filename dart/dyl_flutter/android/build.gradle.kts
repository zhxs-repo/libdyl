// Plugin build script.
//
// Uses the `plugins {}` DSL rather than `buildscript {}` + `apply(plugin=...)`:
// the type-safe accessors (`android { }`, `dependencies { }`, ...) are only
// generated for plugins declared in a `plugins {}` block. With the legacy
// `apply()` form they resolve to nothing under Kotlin DSL, and Gradle 8 fails
// with a wall of "Unresolved reference: android / implementation".
//
// Versions are intentionally omitted: the host app's settings.gradle.kts
// resolves AGP/Kotlin for the whole build, and pinning them here would either
// be ignored or conflict with it.

plugins {
    id("com.android.library")
    id("kotlin-android")
}

group = "dev.dylplayer.dyl_flutter"
version = "0.1.0"

android {
    namespace = "dev.dylplayer.dyl_flutter"
    compileSdk = 34
    // Pinned to match the example app and the aarch64-linux-android26 linker in
    // .cargo/config.toml.
    ndkVersion = "28.2.13676358"

    defaultConfig {
        // 26, not 24: libaaudio.so (AaudioOutput) only exists from API 26 on.
        minSdk = 26

        externalNativeBuild {
            cmake {
                arguments("-DANDROID_STL=c++_shared")
            }
        }
    }

    externalNativeBuild {
        cmake {
            path = file("CMakeLists.txt")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src/main/kotlin")
        }
    }
}

dependencies {
    implementation("org.jetbrains.kotlin:kotlin-stdlib")
}
