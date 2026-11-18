package org.linet

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.core.content.IntentCompat

/** «Поделиться → Отправить на ноутбук»: файлы сервису, текст после подтверждения. */
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uris = when (intent.action) {
            Intent.ACTION_SEND ->
                listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
            Intent.ACTION_SEND_MULTIPLE ->
                IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            else -> emptyList()
        }.filter { Transfers.isForeignContent(this, it) }
        // Поделились текстом, а не файлом: ссылку открываем на ноутбуке, остальное — в его буфер.
        val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
        if (intent.action == Intent.ACTION_SEND && uris.isEmpty() && !text.isNullOrEmpty()) {
            // Exported SEND can be invoked explicitly by any app. Require a real
            // user decision before changing the remote clipboard or opening a URL.
            AlertDialog.Builder(this)
                .setTitle("Отправить текст на ноутбук?")
                .setMessage(text.take(500))
                .setPositiveButton("Отправить") { _, _ ->
                    val url = Handoff.extractUrl(text)
                    if (url != null) {
                        Handoff.send(this, url, intent.getStringExtra(Intent.EXTRA_SUBJECT).orEmpty())
                    } else {
                        ClipboardSync.send(this, text)
                    }
                    finish()
                }
                .setNegativeButton("Отмена") { _, _ -> finish() }
                .setOnCancelListener { finish() }
                .show()
            return
        }
        // Процесс мог только что запуститься: соединение ещё устанавливается, сервис его подождёт.
        val peer = Linet.client(this).paired()
        when {
            uris.isEmpty() -> toast("Нечего отправлять")
            peer == null -> toast("Linet не сопряжён с ноутбуком")
            else -> {
                LinetService.sendFiles(this, uris)
                toast(if (uris.size == 1) "Отправка на ${peer.name}" else "Отправка ${uris.size} файлов на ${peer.name}")
            }
        }
        finish()
    }

    private fun toast(text: String) = Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
}
