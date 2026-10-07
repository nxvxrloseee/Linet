package org.linet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.provider.Settings
import android.util.Log
import androidx.core.content.IntentCompat
import java.io.File

/**
 * Обновление Linet, присланное с ноутбука (`linetctl update-phone`).
 * Android сам проверяет, что новый APK подписан тем же ключом, иначе не установит.
 * Подтвердить установку нужно одним нажатием — так требует система.
 */
object AppUpdate {
    private const val TAG = "LinetUpdate"
    private const val CHANNEL_ID = "update"
    private const val NOTIFICATION_ID = 4

    fun install(context: Context, apk: File) {
        val pm = context.packageManager
        if (!pm.canRequestPackageInstalls()) {
            apk.delete()
            notify(
                context, "Разреши Linet устанавливать обновления",
                "Нажми, включи переключатель и отправь обновление ещё раз",
                Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${context.packageName}")),
            )
            return
        }
        try {
            val installer = pm.packageInstaller
            val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
                .apply { setAppPackageName(context.packageName) }
            val sessionId = installer.createSession(params)
            installer.openSession(sessionId).use { session ->
                session.openWrite("linet.apk", 0, apk.length()).use { out ->
                    apk.inputStream().use { it.copyTo(out) }
                    session.fsync(out)
                }
                val status = PendingIntent.getBroadcast(
                    context, sessionId, Intent(context, InstallReceiver::class.java),
                    // Система дописывает в интент результат установки — он должен быть изменяемым.
                    PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                )
                session.commit(status.intentSender)
            }
        } catch (e: Exception) {
            Log.e(TAG, "install", e)
            notify(context, "Не удалось обновить Linet", e.message ?: e.toString(), null)
        } finally {
            apk.delete()
        }
    }

    fun notify(context: Context, title: String, text: String, tap: Intent?) {
        val nm = context.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL_ID, "Обновления", NotificationManager.IMPORTANCE_HIGH))
        }
        val builder = Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
        if (tap != null) {
            tap.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            builder.setContentIntent(PendingIntent.getActivity(context, 0, tap, PendingIntent.FLAG_IMMUTABLE))
        }
        nm.notify(NOTIFICATION_ID, builder.build())
    }
}

/** Результат установки. Подтверждение из фона открыть нельзя — показываем уведомление. */
class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_INTENT, Intent::class.java) ?: return
                AppUpdate.notify(context, "Обновление Linet готово", "Нажми, чтобы установить", confirm)
            }
            PackageInstaller.STATUS_SUCCESS -> Unit
            else -> {
                val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "код $status"
                AppUpdate.notify(context, "Не удалось обновить Linet", message, null)
            }
        }
    }
}
