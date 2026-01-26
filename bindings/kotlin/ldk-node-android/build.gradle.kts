buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.1.1")
    }
}

plugins {
    kotlin("android") version "2.2.0" apply false
    kotlin("plugin.serialization") version "2.2.0" apply false
}

// library version is defined in gradle.properties
val libraryVersion: String by project

group = "org.lightningdevkit"
version = libraryVersion
