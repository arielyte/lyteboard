# Lyteboard Android development

The app supports arm64-v8a on Android API 26 and later. Gradle builds the Rust library using the NDK and generates Kotlin bindings from the library's UniFFI metadata. The Compose screen can pair with Linux, show connection state, and exchange test text and images.

## CachyOS setup

Install a compatible JDK if you do not already have one. Set `JAVA_HOME` for this shell so other applications keep their existing Java version.

```bash
sudo pacman -S --needed jdk21-openjdk unzip
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk
export PATH="$JAVA_HOME/bin:$PATH"
java -version
```

From the repository root, install the Rust arm64 Android target for the toolchain pinned by Lyteboard.

```bash
rustup target add aarch64-linux-android
```

Download Google's command-line tools to a user-local SDK directory. First review the SDK license, then run the install command and accept the required license if you agree. An existing Android Studio SDK can use the same package-install command with its own `ANDROID_HOME`. [Android SDK downloads and license](https://developer.android.com/studio/#command-tools), [SDK package manager](https://developer.android.com/tools/sdkmanager)

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
sdk_tmp="$(mktemp -d)"
curl --fail --location --output "$sdk_tmp/cmdline.zip" \
  https://dl.google.com/android/repository/commandlinetools-linux-15859902_latest.zip
(
  cd "$sdk_tmp" &&
  printf '%s  cmdline.zip\n' 4e4c464f145a7512b57d088ac6c278c03c9eea610886b35a5e0804e74eedf583 |
    sha256sum --check &&
  unzip -q cmdline.zip &&
  mkdir -p "$ANDROID_HOME/cmdline-tools" &&
  mv cmdline-tools "$ANDROID_HOME/cmdline-tools/latest"
)
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install \
  'platforms;android-36' 'build-tools;36.0.0' 'ndk;28.2.13676358' 'platform-tools'
export PATH="$ANDROID_HOME/platform-tools:$PATH"
```

Gradle 8.14.8 is downloaded and verified by the checked-in wrapper. Gradle resolves build dependencies from Google Maven, Maven Central, and the Gradle Plugin Portal. Cargo uses the workspace lockfile. The NDK build script targets a Linux x86_64 development host. [Gradle release checksums](https://gradle.org/release-checksums/), [Android Gradle Plugin 8.13 compatibility](https://developer.android.com/build/releases/agp-8-13-0-release-notes)

## Build and inspect the debug APK

Run these commands from the repository root.

```bash
cargo check --locked -p lyteboard-android --target aarch64-linux-android
apps/android/gradlew -p apps/android :app:assembleDebug :app:lintDebug
apk=apps/android/app/build/outputs/apk/debug/app-debug.apk
unzip -Z1 "$apk" | grep '^lib/'
"$ANDROID_HOME/build-tools/36.0.0/zipalign" -c -P 16 4 "$apk"
```

The APK should contain `lib/arm64-v8a/liblyteboard_android.so` and `lib/arm64-v8a/libjnidispatch.so`. Android Studio can open `apps/android` directly. Select JDK 21 as the Gradle JDK and launch the IDE with `cargo` on its PATH.

## Install and debug

Use an arm64 Android phone with USB debugging enabled or an emulator whose ABI list includes `arm64-v8a`. If several devices appear, set `ANDROID_SERIAL` to the intended serial. [Connect a hardware device](https://developer.android.com/studio/run/device)

```bash
adb devices -l
adb shell getprop ro.product.cpu.abilist
adb install --abi arm64-v8a -r "$apk"
adb shell am start -W -n app.lyteboard.android/.MainActivity
adb shell uiautomator dump /sdcard/lyteboard-window.xml
adb shell cat /sdcard/lyteboard-window.xml | grep -oE 'Rust core [^"]+|LAN protocol [^"]+'
adb logcat -d -s AndroidRuntime:E
```

The screen should show `Rust core 0.1.0` and `LAN protocol 1` at this checkpoint. Displaying both lines after launch exercises the native call. Use `adb uninstall app.lyteboard.android` to remove the development app.

On a CachyOS x86_64 host with KVM, the Android 16 Google APIs emulator can run the arm64 APK through Android's native bridge. Review any SDK license prompts before installing the image. [Android Emulator ARM support](https://developer.android.com/studio/releases/emulator)

```bash
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install \
  emulator 'system-images;android-36;google_apis;x86_64'
"$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd \
  --name lyteboard-lan6a --package 'system-images;android-36;google_apis;x86_64'
"$ANDROID_HOME/emulator/emulator" -avd lyteboard-lan6a -port 5562 -no-snapshot
```

Run the ADB commands from a second shell with `ANDROID_SERIAL=emulator-5562` exported. Stop that emulator with `adb -s emulator-5562 emu kill`.

## Pair and exchange test content

On Linux, bind to the actual Wi-Fi or Ethernet IP on the same subnet as Android. Use an isolated Linux profile if testing revocation. Keep the invitation private until you paste it into Lyteboard.

```bash
lyteboard daemon start
read -r -p 'Linux LAN IPv4 address: ' linux_ip
lyteboard network listen "$linux_ip:47851"
lyteboard pair invite
# Paste the invitation into Android and tap Import invitation.
lyteboard pair status
# Compare both screens, enter the Linux code on Android, and tap Approve.
lyteboard pair confirm
# Enter the code displayed on Android.
lyteboard devices list
read -r -p 'Android device ID: ' android_id
lyteboard send "$android_id" 'Hello from Linux ✓'
wl-copy --type image/png --sensitive < sample.png
lyteboard clipboard send "$android_id"
sha256sum sample.png
```

Use **View text** or **Save received item** on Android. Compare the displayed SHA-256 with the Linux hash. Select Linux in **Trusted devices**, then use **Send text** or **Send image**. For an Android image sent to Linux, run `lyteboard clipboard copy` and `wl-paste --no-newline --type image/png | sha256sum`. Use the matching MIME type for JPEG or WebP.

Android initiates the session. Both peers can send once it is connected. Reopen the Android app to reconnect or select the device and enter a numeric IP and port. Android NSD provides address hints for already paired peers. Android's `INTERNET` manifest permission enables LAN sockets at target SDK 36. Dials are restricted to local interface subnets and verified against the stored Noise pin. [Android LAN permissions](https://developer.android.com/privacy-and-security/local-network-permission)

For automated verification, start a disposable API 36 emulator with empty Lyteboard app data and an `adb` executable on PATH. On Linux, install `dbus` and `gnome-keyring` if absent. From the repository root run:

```bash
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
apps/android/gradlew -p apps/android :app:assembleDebug :app:assembleDebugAndroidTest :app:lintDebug
PYTHONDONTWRITEBYTECODE=1 python3 apps/android/tests/lan_emulator.py emulator-5562
```

The test runner is in the separate instrumentation APK. It exchanges data with two real Linux daemons. Linux clipboard input and output use an isolated file-backed test adapter. The emulator reaches Linux host loopback at `10.0.2.2`, so the harness changes only the address hint in Linux-generated invitations.
