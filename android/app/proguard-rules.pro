# JNA и сгенерированные uniffi-биндинги работают через рефлексию.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class uniffi.** { *; }
-dontwarn java.awt.**

# ML Kit (сканер QR) находит свои компоненты через рефлексию; R8 в полном режиме
# вырезает их, и GmsBarcodeScanning.getClient падает с NullPointerException.
-keep class * implements com.google.firebase.components.ComponentRegistrar { *; }
-keep class com.google.mlkit.** { *; }
-keep class com.google.android.gms.internal.mlkit_code_scanner.** { *; }
-keep class com.google.android.gms.internal.mlkit_vision_common.** { *; }
