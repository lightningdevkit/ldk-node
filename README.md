# ldk-lite
A Simplified API for LDK.

## Build and publish to local Maven repository
```shell
source uniffi_bindgen_generate_kotlin.sh
cd ldk-node-jvm
./gradlew publishToMavenLocal
```

## How to Use
To use the Kotlin language bindings for [`ldk-node`] in your JVM project, add the following to your gradle dependencies:
```kotlin
repositories {
    mavenCentral()
}

dependencies {
    implementation("org.ldk:ldk-node:0.0.1")
}
```

You may then import and use the `org.ldk_node` library in your Kotlin code. For example:
```kotlin
import uniffi.ldk_node.Builder
import uniffi.ldk_node.Node

fun main() {
    val node: Node = Builder().build()
}
```
