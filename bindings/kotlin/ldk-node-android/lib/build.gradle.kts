import org.gradle.api.tasks.testing.logging.TestExceptionFormat.*
import org.gradle.api.tasks.testing.logging.TestLogEvent.*

// library version is defined in gradle.properties
val libraryVersion: String by project

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android") version "1.6.10"

    id("maven-publish")
    id("signing")
}

repositories {
    mavenCentral()
    google()
}

android {
    compileSdk = 31

    defaultConfig {
        minSdk = 21
        targetSdk = 31
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
            proguardFiles(file("proguard-android-optimize.txt"), file("proguard-rules.pro"))
        }
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
            withJavadocJar()
        }
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.8.0@aar")
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk7")
    implementation("androidx.appcompat:appcompat:1.4.0")
    implementation("androidx.core:core-ktx:1.7.0")
    api("org.slf4j:slf4j-api:1.7.30")

    androidTestImplementation("com.github.tony19:logback-android:2.0.0")
    androidTestImplementation("androidx.test.ext:junit:1.1.3")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.4.0")
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.4.1")
    androidTestImplementation("org.jetbrains.kotlin:kotlin-test-junit")
}

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                groupId = "co.jurvis"
                artifactId = "ldk-node-android"
                version = libraryVersion

                from(components["release"])
                pom {
                    name.set("ldk-node-android")
                    description.set(
                        "Kotlin language bindings for LdkNode, a ready-to-go LDK node implementation"
                    )
                    url.set("https://lightningdevkit.org")
                    licenses {
                        license {
                            name.set("APACHE 2.0")
                            url.set("https://github.com/lightningdevkit/ldk-node/blob/main/LICENSE-APACHE")
                        }
                        license {
                            name.set("MIT")
                            url.set("https://github.com/lightningdevkit/ldk-node/blob/main/LICENSE-MIT")
                        }
                    }
                    developers {
                        developer {
                            id.set("tnull")
                            name.set("Elias Rohrer")
                            email.set("tnull@noreply.github.org")
                        }
                        developer {
                            id.set("jurvis")
                            name.set("Jurvis Tan")
                            email.set("jurvis@noreply.github.org")
                        }
                    }
                    scm {
                        connection.set("scm:git:github.com/jurvis/ldk-node.git")
                        developerConnection.set("scm:git:ssh://github.com/jurvis/ldk-node.git")
                        url.set("https://github.com/jurvis/ldk-node/tree/main")
                    }
                }
            }
        }
    }
}

signing {
    val signingKeyId: String? by project
    val signingKey: String? by project
    val signingPassword: String? by project
    useInMemoryPgpKeys(signingKeyId, signingKey, signingPassword)
    sign(publishing.publications)
}

//tasks.named<Test>("test") {
//    // Use JUnit Platform for unit tests.
//    useJUnitPlatform()
//
//	testLogging {
//        events(PASSED, SKIPPED, FAILED, STANDARD_OUT, STANDARD_ERROR)
//        exceptionFormat = FULL
//        showExceptions = true
//        showCauses = true
//        showStackTraces = true
//		showStandardStreams = true
//    }
//}

//// This task dependency ensures that we build the bindings
//// binaries before running the tests
//tasks.withType<KotlinCompile> {
//    dependsOn("buildAndroidLib")
//}
