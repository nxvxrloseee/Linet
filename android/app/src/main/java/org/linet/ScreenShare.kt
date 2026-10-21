package org.linet

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.provider.Settings
import android.util.Log

/**
 * Беспроводная отладка для scrcpy (экран телефона на ноутбуке).
 * Включается только по запросу ноутбука и выключается после трансляции или при
 * обрыве связи — в остальное время ADB по сети закрыт.
 * Нужно разрешение WRITE_SECURE_SETTINGS, его выдают один раз по USB:
 * `adb shell pm grant org.linet android.permission.WRITE_SECURE_SETTINGS`.
 */
object ScreenShare {
    private const val TAG = "LinetScreen"
    private const val ADB_WIFI = "adb_wifi_enabled"

    @Volatile
    private var enabledByUs = false

    /** Возвращает текст ошибки или null. */
    fun set(context: Context, enable: Boolean): String? {
        if (context.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) != PackageManager.PERMISSION_GRANTED) {
            return "нет разрешения: выполни по USB «adb shell pm grant org.linet android.permission.WRITE_SECURE_SETTINGS»"
        }
        if (enable && !onWifi(context)) {
            return "беспроводная отладка работает только в Wi-Fi сети, а не в точке доступа"
        }
        return try {
            Settings.Global.putInt(context.contentResolver, ADB_WIFI, if (enable) 1 else 0)
            enabledByUs = enable
            Log.i(TAG, "беспроводная отладка: $enable")
            null
        } catch (e: SecurityException) {
            e.message ?: "нет доступа к настройке"
        }
    }

    /** Связь с ноутбуком пропала — не оставлять отладку открытой. */
    fun disconnected(context: Context) {
        if (enabledByUs) set(context, false)
    }

    private fun onWifi(context: Context): Boolean {
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
    }
}
