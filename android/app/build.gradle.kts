plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val releaseKeystore = providers.environmentVariable("ANDROID_KEYSTORE_FILE").orNull?.takeIf { it.isNotBlank() }
val releaseStorePassword = providers.environmentVariable("ANDROID_KEYSTORE_PASSWORD").orNull?.takeIf { it.isNotBlank() }
val releaseKeyAlias = providers.environmentVariable("ANDROID_KEY_ALIAS").orNull?.takeIf { it.isNotBlank() }
val releaseKeyPassword = providers.environmentVariable("ANDROID_KEY_PASSWORD").orNull?.takeIf { it.isNotBlank() }
val canSignRelease = listOf(releaseKeystore, releaseStorePassword, releaseKeyAlias, releaseKeyPassword).all { it != null }
val unsignedPullRequest = providers.environmentVariable("GITHUB_EVENT_NAME").orNull == "pull_request"

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

    val releaseSigning = if (canSignRelease) signingConfigs.create("distribution") {
        storeFile = file(checkNotNull(releaseKeystore))
        storePassword = releaseStorePassword
        keyAlias = releaseKeyAlias
        keyPassword = releaseKeyPassword
    } else null

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = releaseSigning
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

tasks.matching { it.name == "packageRelease" }.configureEach {
    inputs.property("unsignedPullRequest", unsignedPullRequest)
    doFirst {
        check(canSignRelease || unsignedPullRequest) {
            "Release APKs require ANDROID_KEYSTORE_FILE, ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS, and ANDROID_KEY_PASSWORD."
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.games:games-activity:3.0.5")
}
