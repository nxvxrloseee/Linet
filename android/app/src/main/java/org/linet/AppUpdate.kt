package org.linet

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.core.content.FileProvider
import java.io.File

/**
 * Обновление Linet, присланное с ноутбука (`linetctl update-phone`).
 *
 * APK кладётся в кэш, а ставит его системный установщик — тот же путь, что
 * «открыть APK из файлового менеджера». Запускается он из открытого Linet: из фона
 * HyperOS окно установки молча не показывает. Android сам проверяет, что новый APK
 * подписан тем же ключом, иначе не установит.
 */
object AppUpdate {
    private const val TAG = "LinetUpdate"
    private const val CHANNEL_ID = "update"
    private const val NOTIFICATION_ID = 4
    const val EXTRA_INSTALL = "org.linet.INSTALL_UPDATE"
    private const val AUTHORITY = "org.linet.clip"

    private fun file(context: Context) = File(File(context.cacheDir, "update"), "linet-update.apk")

    /** Есть скачанное, но ещё не установленное обновление. */
    fun pending(context: Context): Boolean {
        val f = file(context)
        if (!f.exists()) return false
        // Обновление установилось, если приложение обновлено позже, чем скачан APK.
        val updatedAt = context.packageManager.getPackageInfo(context.packageName, 0).lastUpdateTime
        if (updatedAt > f.lastModified()) {
            f.delete()
            return false
        }
        return true
    }

    /** Вызывается после приёма APK с ноутбука. */
    fun received(context: Context, apk: File) {
        val target = file(context).apply { parentFile?.mkdirs() }
        if (!apk.renameTo(target)) {
            apk.copyTo(target, overwrite = true)
            apk.delete()
        }
        val open = Intent(context, MainActivity::class.java)
            .putExtra(EXTRA_INSTALL, true)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        notify(context, "Обновление Linet готово", "Нажми, чтобы установить", open)
    }

    /** Открыть системный установщик. Только из видимого Activity. */
    fun install(activity: Activity) {
        if (!activity.packageManager.canRequestPackageInstalls()) {
            Toast.makeText(activity, "Разреши Linet устанавливать приложения и нажми ещё раз", Toast.LENGTH_LONG).show()
            activity.startActivity(
                Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${activity.packageName}")),
            )
            return
        }
        val f = file(activity)
        if (!f.exists()) {
            Toast.makeText(activity, "Обновление не найдено, отправь его ещё раз", Toast.LENGTH_LONG).show()
            return
        }
        activity.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
        val uri = FileProvider.getUriForFile(activity, AUTHORITY, f)
        val view = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        try {
            activity.startActivity(view)
        } catch (e: Exception) {
            Log.e(TAG, "install", e)
            Toast.makeText(activity, "Не удалось открыть установщик: ${e.message}", Toast.LENGTH_LONG).show()
        }
    }

    private fun notify(context: Context, title: String, text: String, tap: Intent) {
        val nm = context.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL_ID, "Обновления", NotificationManager.IMPORTANCE_HIGH))
        }
        val notification = Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
            .setContentIntent(PendingIntent.getActivity(context, 0, tap, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
            .build()
        nm.notify(NOTIFICATION_ID, notification)
    }
}
