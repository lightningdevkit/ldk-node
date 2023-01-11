import org.gradle.api.tasks.testing.logging.TestExceptionFormat.*
import org.gradle.api.tasks.testing.logging.TestLogEvent.*

// library version is defined in gradle.properties
val libraryVersion: String by project

plugins {
    id("org.jetbrains.kotlin.jvm") version "1.5.31"
    id("java-library")
    id("maven-publish")
}

repositories {
    mavenCentral()
}

java {
    sourceCompatibility = JavaVersion.VERSION_1_8
    targetCompatibility = JavaVersion.VERSION_1_8
    withSourcesJar()
    withJavadocJar()
}

tasks.withType<Test> {
    useJUnitPlatform()

    testLogging {
        events(PASSED, SKIPPED, FAILED, STANDARD_OUT, STANDARD_ERROR)
        exceptionFormat = FULL
        showExceptions = true
        showCauses = true
        showStackTraces = true
    }
}

dependencies {
    // Align versions of all Kotlin components
    implementation(platform("org.jetbrains.kotlin:kotlin-bom"))

    // Use the Kotlin JDK 8 standard library.
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk8")

    implementation("net.java.dev.jna:jna:5.8.0")
}

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                groupId = "org.ldk"
                artifactId = "ldk-node"
                version = libraryVersion

                from(components["java"])
                pom {
                    name.set("ldk-node")
                    description.set("Lightning Dev Kit Node language bindings for Kotlin.")
                    url.set("https://lightningdevkit.org")
                    licenses {
                        license {
                            name.set("APACHE 2.0")
                            url.set("https://github.com/bitcoindevkit/bdk/blob/master/LICENSE-APACHE")
                        }
                        license {
                            name.set("MIT")
                            url.set("https://github.com/bitcoindevkit/bdk/blob/master/LICENSE-MIT")
                        }
                    }
                    developers {
                        developer {
                            id.set("")
                            name.set("")
                            email.set("")
                        }
                        developer {
                            id.set("")
                            name.set("")
                            email.set("")
                        }
                    }
                    scm {
                        connection.set("scm:git:github.com/lightningdevkit/ldk-node.git")
                        developerConnection.set("scm:git:ssh://github.com/lightningdevkit/ldk-node.git")
                        url.set("https://github.com/lightningdevkit/ldk-node/tree/master")
                    }
                }
            }
        }
    }
}

testing {
    suites {
        // Configure the built-in test suite
        val test by getting(JvmTestSuite::class) {
            // Use Kotlin Test test framework
            useKotlinTest()
        }
    }
}
