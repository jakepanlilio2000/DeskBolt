# cargo-ndk sets these for each ABI and Android API level.
set(ANDROID_ABI "$ENV{CARGO_NDK_ANDROID_PLATFORM}")
set(ANDROID_PLATFORM "android-$ENV{ANDROID_PLATFORM}")
include("$ENV{CARGO_NDK_CMAKE_TOOLCHAIN_PATH}")
