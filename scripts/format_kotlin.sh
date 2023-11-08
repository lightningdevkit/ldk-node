#!/bin/bash
LDK_NODE_ANDROID_DIR="bindings/kotlin/ldk-node-android"
LDK_NODE_JVM_DIR="bindings/kotlin/ldk-node-jvm"

# Run ktlintFormat in ldk-node-android
(
  cd $LDK_NODE_ANDROID_DIR || exit 1
  ./gradlew ktlintFormat
)

# Run ktlintFormat in ldk-node-jvm
(
  cd $LDK_NODE_JVM_DIR || exit 1
  ./gradlew ktlintFormat
)
