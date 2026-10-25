import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Версия растёт сама: versionCode — число коммитов, versionName — версия из Cargo.toml
// и этот номер (например, 0.1.1-57). Так каждое обновление отличимо от прошлого.
val linetVersionCode: Int = providers.exec {
    commandLine("git", "rev-list", "--count", "HEAD")
    isIgnoreExitValue = true
}.standardOutput.asText.get().trim().toIntOrNull()?.plus(100) ?: 1
val linetVersionName: String = run {
    val cargo = rootProject.file("../Cargo.toml").readText()
    val base = Regex("""(?m)^version\s*=\s*"([^"]+)"""").find(cargo)?.groupValues?.get(1) ?: "0.0.0"
    "$base-$linetVersionCode"
}

// Ключ подписи лежит вне репозитория: ~/.config/linet/signing.properties (+ .jks рядом).
// Без него release подписывается debug-ключом, и обновить установленное приложение не выйдет.
val signingProps = Properties().apply {
    val file = File(System.getProperty("user.home"), ".config/linet/signing.properties")
    if (file.exists()) file.inputStream().use { load(it) }
}

android {
    namespace = "org.linet"
    compileSdk { version = release(37) { minorApiLevel = 2 } }

    defaultConfig {
        applicationId = "org.linet"
        minSdk = 29
        targetSdk = 36
        versionCode = linetVersionCode
        versionName = linetVersionName
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    signingConfigs {
        if (signingProps.isNotEmpty()) {
            create("linet") {
                storeFile = File(signingProps.getProperty("storeFile"))
                storePassword = signingProps.getProperty("storePassword")
                keyAlias = signingProps.getProperty("keyAlias")
                keyPassword = signingProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        debug {
            signingConfigs.findByName("linet")?.let { signingConfig = it }
        }
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("linet") ?: signingConfigs.getByName("debug")
        }
    }

    buildFeatures { compose = true }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(composeBom)
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.core:core-ktx:1.19.1")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.11.0")
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
