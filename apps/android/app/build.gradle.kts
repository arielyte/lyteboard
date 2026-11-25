import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application") version "8.13.2"
    kotlin("android") version "2.3.10"
    id("org.jetbrains.kotlin.plugin.compose") version "2.3.10"
}

val minApi = 26
val bindingsDir = layout.buildDirectory.dir("generated/uniffi")
val nativeLibsDir = layout.buildDirectory.dir("generated/jniLibs")

android {
    namespace = "app.lyteboard.android"
    compileSdk = 36
    buildToolsVersion = "36.0.0"
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "app.lyteboard.android"
        minSdk = minApi
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk.abiFilters += "arm64-v8a"
    }

    buildFeatures { compose = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    sourceSets.named("main") {
        java.srcDir(bindingsDir)
        jniLibs.srcDir(nativeLibsDir)
    }
}

kotlin {
    compilerOptions.jvmTarget.set(JvmTarget.JVM_17)
}

dependencies {
    implementation("androidx.activity:activity-compose:1.11.0")
    implementation("androidx.compose.material3:material3:1.4.0")
    implementation("net.java.dev.jna:jna:5.18.1@aar")
}

val ndkDirectory = androidComponents.sdkComponents.ndkDirectory
val buildRust by tasks.registering(Exec::class) {
    workingDir(rootProject.projectDir)
    executable = "bash"
    val script = rootProject.file("build-native.sh")
    // Cargo tracks the Rust sources, profiles, and toolchain for both builds.
    argumentProviders.add(CommandLineArgumentProvider {
        listOf(
            script.absolutePath,
            ndkDirectory.get().asFile.absolutePath,
            minApi.toString(),
            bindingsDir.get().asFile.absolutePath,
            nativeLibsDir.get().asFile.absolutePath,
        )
    })
}

tasks.named("preBuild") { dependsOn(buildRust) }
