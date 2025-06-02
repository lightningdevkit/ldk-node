## Publishing
Publishing new version guide.

1. Run in root dir
   `sh scripts/uniffi_bindgen_generate_kotlin_android.sh`

1. Update `libraryVersion` in `bindings/kotlin/ldk-node-android/gradle.properties`.

1. Commit

1. Push new branch (or new tag)

1. Go to [Jitpack repo url](https://jitpack.io/#synonymdev/ldk-node),
    choose the build type by tab (ie. releases, branch, etc.), and tap 'Get it'
    for the required version.

    - If a build already exists, tap the file icon under 'Log' to check the status.

1. In the android project:

    - in `settings.gradle.kts` add jitpack repository using `maven("https://jitpack.io")`:

        ```kt
        dependencyResolutionManagement {
            repositories {
                google()
                mavenCentral()
                maven("https://jitpack.io")
            }
        ```
    - add dependency in `libs.versions.toml`:
        ```toml
        # by tag
        ldk-node-android = { module = "com.github.ovitrif:ldk-node", version = "v0.6.0-rc.1"

        # or by branch
        ldk-node-android = { module = "com.github.ovitrif:ldk-node", version = "main-SNAPSHOT"
        ```
    - Run `Sync project with gradle files` action in android studio
