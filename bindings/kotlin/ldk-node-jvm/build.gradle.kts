buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
    }
}

plugins {
}

// library version is defined in gradle.properties
val libraryVersion: String by project

group = "org.lightningdevkit"
version = libraryVersion
