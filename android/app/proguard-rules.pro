# JNA и сгенерированные uniffi-биндинги работают через рефлексию.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class uniffi.** { *; }
-dontwarn java.awt.**
