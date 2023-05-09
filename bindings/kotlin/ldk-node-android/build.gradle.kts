buildscript {
    repositories {
        google()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:7.1.2")
    }
}

plugins {
    id("io.github.gradle-nexus.publish-plugin") version "1.1.0"
}

// library version is defined in gradle.properties
val libraryVersion: String by project

version = libraryVersion
