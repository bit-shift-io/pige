// Adapted from qlaunch (bitshift, MIT).
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The shared test-phone key is committed at android/keystore/ so every PC signs
// identically; `android/keystore.properties` supplies it.
val keystoreProperties = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.exists()) file.inputStream().use { load(it) }
}
val hasSharedKey = keystoreProperties.getProperty("storeFile") != null

android {
    namespace = "bitshift.pige"
    compileSdk = 36
    ndkVersion = "28.2.13676358"

    // Supplied by scripts/build.sh from Cargo's resolved pinned dependency.
    sourceSets.getByName("main").java.setSrcDirs(listOf(
        "src/main/kotlin", providers.gradleProperty("icedAndroidJava").get()
    ))

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "bitshift.pige"
        minSdk = 34
        targetSdk = 36
        // Supplied by scripts/build.sh from the workspace Cargo version.
        // Fallbacks are deliberately obvious so a bare Gradle build is unversioned.
        versionCode = providers.gradleProperty("qlaunchVersionCode").getOrElse("1").toInt()
        versionName = providers.gradleProperty("qlaunchVersionName").getOrElse("0.0.0")
        ndk { abiFilters += "arm64-v8a" }
    }

    signingConfigs {
        if (hasSharedKey) {
            create("shared") {
                storeFile = rootProject.file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        // Sign development builds with the shared key too, so installs from
        // different machines replace one another instead of failing with a
        // signature mismatch. The first install after switching keys needs an
        // uninstall because the old APK used the local debug key.
        debug {
            signingConfig = if (hasSharedKey) {
                signingConfigs.getByName("shared")
            } else {
                signingConfigs.getByName("debug")
            }
        }
        release {
            // A missing keystore.properties falls back to the local debug key.
            signingConfig = if (hasSharedKey) {
                signingConfigs.getByName("shared")
            } else {
                signingConfigs.getByName("debug")
            }
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.work:work-runtime:2.11.2")
    testImplementation("junit:junit:4.13.2")
}
