package org.linet

import android.annotation.SuppressLint
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings

/** Исключения из энергосбережения. На Xiaomi без них сервис убивают в фоне. */
object Power {
    val isXiaomi: Boolean
        get() = Build.MANUFACTURER.equals("Xiaomi", ignoreCase = true) ||
            Build.BRAND.lowercase() in setOf("xiaomi", "redmi", "poco")

    fun isIgnoringOptimizations(context: Context): Boolean =
        context.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(context.packageName)

    @SuppressLint("BatteryLife") // Приложение для личного использования, не для Play
    fun requestIgnoreOptimizations(context: Context) {
        context.startActivity(
            Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${context.packageName}")),
        )
    }

    /** Экран «Автозапуск» в MIUI / HyperOS. */
    fun openXiaomiAutostart(context: Context) {
        val intent = Intent().setComponent(
            ComponentName("com.miui.securitycenter", "com.miui.permcenter.autostart.AutoStartManagementActivity"),
        )
        runCatching { context.startActivity(intent) }.onFailure { openAppSettings(context) }
    }

    fun openAppSettings(context: Context) {
        context.startActivity(
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${context.packageName}")),
        )
    }
}
