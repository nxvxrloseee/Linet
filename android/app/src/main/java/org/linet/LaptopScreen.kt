package org.linet

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.widget.Toast
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import uniffi.linet_ffi.LinetException

/**
 * Экран ноутбука на телефоне: ноутбук запускает wayvnc с одноразовым паролем,
 * а ссылку `vnc://…` открывает VNC-клиент (AVNC понимает такие ссылки с паролем).
 */
object LaptopScreen {
    private const val AVNC = "com.gaurav.avnc"

    suspend fun open(activity: Activity, extend: Boolean) {
        // Второй монитор — по размеру и ориентации экрана телефона прямо сейчас.
        val bounds = activity.windowManager.currentWindowMetrics.bounds
        val (w, h) = bounds.width() to bounds.height()
        val url = withContext(Dispatchers.IO) {
            try {
                Result.success(Linet.client(activity).requestVnc(extend, w.toUInt(), h.toUInt()))
            } catch (e: LinetException) {
                Result.failure(e)
            }
        }
        url.onFailure {
            Toast.makeText(activity, "Не вышло: ${it.message}", Toast.LENGTH_LONG).show()
            return
        }
        val uri = Uri.parse(url.getOrThrow())
        if (uri.scheme != "vnc" || uri.host.isNullOrBlank() || uri.userInfo != null) {
            Linet.client(activity).stopVnc()
            Toast.makeText(activity, "Некорректная ссылка VNC", Toast.LENGTH_LONG).show()
            return
        }
        try {
            activity.startActivity(Intent(Intent.ACTION_VIEW, uri))
        } catch (_: ActivityNotFoundException) {
            Linet.client(activity).stopVnc()
            Toast.makeText(activity, "Нужен VNC-клиент: установи AVNC", Toast.LENGTH_LONG).show()
            runCatching {
                activity.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("market://details?id=$AVNC")))
            }
        }
    }
}
