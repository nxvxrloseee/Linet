package org.linet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Environment
import android.os.PowerManager
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import android.webkit.MimeTypeMap
import uniffi.linet_ffi.TransferInfo
import uniffi.linet_ffi.TransferStatus
import java.io.File
import java.util.concurrent.Executors

/**
 * Ход передач: уведомления, сохранение принятых файлов в «Загрузки/Linet»
 * и WakeLock только на время активной передачи.
 */
object Transfers {
    private const val TAG = "LinetTransfers"
    private const val CHANNEL_ID = "transfers"
    private const val WAKE_TIMEOUT_MS = 10 * 60 * 1000L

    /** Колбэки приходят из единственного рабочего потока Rust — не блокируем его. */
    private val worker = Executors.newSingleThreadExecutor()
    private val active = HashSet<ULong>()
    private var wakeLock: PowerManager.WakeLock? = null

    fun onUpdate(context: Context, t: TransferInfo) {
        val app = context.applicationContext
        worker.execute {
            runCatching { handle(app, t) }.onFailure { Log.e(TAG, "transfer ${t.name}", it) }
        }
    }

    private fun handle(context: Context, t: TransferInfo) {
        val nm = context.getSystemService(NotificationManager::class.java)
        ensureChannel(nm)
        when (val s = t.status) {
            is TransferStatus.Progress -> {
                if (active.add(t.id)) setAwake(context, true)
                nm.notify(notificationId(t), progressNotification(context, t))
            }
            is TransferStatus.Done -> {
                finish(context, t)
                val saved = s.path?.let { saveToDownloads(context, File(it), t.name) }
                nm.notify(notificationId(t), doneNotification(context, t, saved))
            }
            is TransferStatus.Failed -> {
                finish(context, t)
                nm.notify(notificationId(t), base(context, "Не удалось: ${t.name}").setContentText(s.reason).build())
            }
        }
    }

    private fun finish(context: Context, t: TransferInfo) {
        active.remove(t.id)
        if (active.isEmpty()) setAwake(context, false)
    }

    /** Пока идёт передача, CPU не должен засыпать, иначе поток данных встанет. */
    private fun setAwake(context: Context, awake: Boolean) {
        if (awake) {
            val lock = wakeLock ?: context.getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "linet:transfer")
                .apply { setReferenceCounted(false) }
                .also { wakeLock = it }
            lock.acquire(WAKE_TIMEOUT_MS)
        } else {
            wakeLock?.takeIf { it.isHeld }?.release()
        }
    }

    /** Переносит файл из кэша приложения в общие «Загрузки/Linet». */
    private fun saveToDownloads(context: Context, file: File, name: String): Uri? {
        val mime = mimeOf(name)
        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, name)
            put(MediaStore.Downloads.MIME_TYPE, mime)
            put(MediaStore.Downloads.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/Linet")
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
        val resolver = context.contentResolver
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: return null
        try {
            resolver.openOutputStream(uri)!!.use { out -> file.inputStream().use { it.copyTo(out) } }
            resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
            return uri
        } catch (e: Exception) {
            resolver.delete(uri, null, null)
            throw e
        } finally {
            file.delete()
        }
    }

    fun mimeOf(name: String): String =
        MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase())
            ?: "application/octet-stream"

    private fun notificationId(t: TransferInfo): Int = 1000 + (t.id % 1_000_000uL).toInt()

    private fun ensureChannel(nm: NotificationManager) {
        if (nm.getNotificationChannel(CHANNEL_ID) != null) return
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Передача файлов", NotificationManager.IMPORTANCE_LOW),
        )
    }

    private fun base(context: Context, title: String) = Notification.Builder(context, CHANNEL_ID)
        .setSmallIcon(R.drawable.ic_linet)
        .setContentTitle(title)

    private fun progressNotification(context: Context, t: TransferInfo): Notification {
        val title = if (t.incoming) "Приём от ${t.peer}" else "Отправка на ${t.peer}"
        val max = 1000
        val progress = if (t.total > 0uL) (t.done * max.toULong() / t.total).toInt() else 0
        return base(context, title)
            .setContentText(t.name)
            .setProgress(max, progress, t.total == 0uL)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()
    }

    private fun doneNotification(context: Context, t: TransferInfo, saved: Uri?): Notification {
        val b = base(context, if (t.incoming) "Файл от ${t.peer}" else "Отправлено на ${t.peer}")
            .setContentText(t.name)
            .setAutoCancel(true)
        if (saved != null) {
            val view = Intent(Intent.ACTION_VIEW).setDataAndType(saved, mimeOf(t.name))
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
            b.setContentIntent(PendingIntent.getActivity(context, notificationId(t), view, PendingIntent.FLAG_IMMUTABLE))
        }
        return b.build()
    }

    /** Отправка одного URI. Блокирует до доставки — вызывать с фонового потока. */
    fun send(context: Context, uri: Uri) {
        val resolver = context.contentResolver
        var name = uri.lastPathSegment ?: "file"
        var size = -1L
        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use {
            if (it.moveToFirst()) {
                if (!it.isNull(0)) name = it.getString(0)
                if (!it.isNull(1)) size = it.getLong(1)
            }
        }
        val pfd = resolver.openFileDescriptor(uri, "r") ?: error("не удалось открыть $uri")
        if (size < 0) size = pfd.statSize
        if (size < 0) {
            pfd.close()
            error("неизвестный размер файла $name")
        }
        val mime = resolver.getType(uri) ?: mimeOf(name)
        Linet.client(context).sendFd(pfd.detachFd(), name, size.toULong(), mime)
    }
}
