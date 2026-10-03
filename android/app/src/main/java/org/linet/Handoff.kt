package org.linet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.widget.Toast

/**
 * «Продолжить на другом устройстве».
 * Открыть страницу из фона Android не даст, поэтому ссылка с ноутбука
 * приходит уведомлением: нажал — открылась в браузере.
 */
object Handoff {
    private const val CHANNEL_ID = "handoff"
    private const val NOTIFICATION_ID = 2

    /** Вызывается из потока Rust. */
    fun show(context: Context, url: String, title: String) {
        val uri = Uri.parse(url)
        if (uri.scheme != "https" && uri.scheme != "http") return
        val nm = context.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "Страницы с ноутбука", NotificationManager.IMPORTANCE_HIGH),
            )
        }
        val open = Intent(Intent.ACTION_VIEW, uri).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val notification = Notification.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle(title.ifBlank { "Страница с ноутбука" })
            .setContentText(uri.host ?: url)
            .setContentIntent(PendingIntent.getActivity(context, 0, open, PendingIntent.FLAG_IMMUTABLE))
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_RECOMMENDATION)
            .build()
        nm.notify(NOTIFICATION_ID, notification)
    }

    /** Текст из «Поделиться» — это одна ссылка (браузеры делятся именно так)? */
    fun extractUrl(text: String): String? {
        val trimmed = text.trim()
        val candidate = trimmed.lines().last().trim()
        val isUrl = (candidate.startsWith("https://") || candidate.startsWith("http://")) &&
            candidate.none { it.isWhitespace() }
        // Chrome шлёт «заголовок\nссылка», Firefox — только ссылку.
        return candidate.takeIf { isUrl && trimmed.lines().size <= 2 }
    }

    fun send(context: Context, url: String, title: String) {
        val ok = Linet.client(context).sendHandoff(url, title)
        Toast.makeText(context, if (ok) "Открыто на ноутбуке" else "Ноутбук не подключён", Toast.LENGTH_SHORT).show()
    }
}
