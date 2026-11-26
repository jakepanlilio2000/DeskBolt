plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.stacksync.deskbolt"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.stacksync.deskbolt"
        minSdk = 26
        targetSdk = 35
        versionCode = (project.findProperty("deskboltVersionCode")?.toString()?.toIntOrNull()) ?: 1
        versionName = (project.findProperty("deskboltVersionName")?.toString()) ?: "0.1.0-beta1"
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    packaging {
        jniLibs {
            // Keep uncompressed 16 KB aligned .so files for Android 15/16 page-size compatibility
            useLegacyPackaging = false
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.games:games-activity:3.0.5")
}
