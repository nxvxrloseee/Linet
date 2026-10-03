package org.linet

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.linet_ffi.LinetException

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        if (Linet.client(this).paired() != null) LinetService.start(this)
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
        }
        setContent { LinetTheme { Surface(Modifier.fillMaxSize()) { Screen() } } }
    }
}

@Composable
private fun LinetTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    val colors = when {
        Build.VERSION.SDK_INT >= 31 && dark -> dynamicDarkColorScheme(context)
        Build.VERSION.SDK_INT >= 31 -> dynamicLightColorScheme(context)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = colors, content = content)
}

@Composable
private fun Screen() {
    val status by Linet.status.collectAsStateWithLifecycle()
    Column(
        Modifier.safeDrawingPadding().padding(16.dp).verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Linet", style = MaterialTheme.typography.headlineMedium)
        when (val s = status) {
            Status.NotPaired -> PairCard()
            is Status.Offline -> PeerCard(s.peer.name, connected = false, detail = s.reason)
            is Status.Connected -> PeerCard(s.peer.name, connected = true, detail = null)
        }
        if (status !is Status.NotPaired) NotificationsCard()
        PowerCard()
    }
}

private suspend fun <T> io(block: () -> T): Result<T> = withContext(Dispatchers.IO) {
    try {
        Result.success(block())
    } catch (e: LinetException) {
        Result.failure(e)
    }
}

@Composable
private fun PairCard() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var uri by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }

    fun pair(value: String) {
        busy = true
        error = null
        scope.launch {
            io { Linet.client(context).pair(value.trim()) }
                .onSuccess { LinetService.start(context) }
                .onFailure { error = it.message }
            busy = false
        }
    }

    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Сопряжение", style = MaterialTheme.typography.titleMedium)
            Text("Запусти на ноутбуке `linetctl pair` и отсканируй QR-код.")
            Button(
                enabled = !busy,
                onClick = {
                    val options = GmsBarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build()
                    GmsBarcodeScanning.getClient(context, options).startScan()
                        .addOnSuccessListener { code -> code.rawValue?.let(::pair) }
                        .addOnFailureListener { error = it.message }
                },
            ) { Text(if (busy) "Подключение…" else "Сканировать QR") }
            OutlinedTextField(
                value = uri,
                onValueChange = { uri = it },
                label = { Text("или вставь ссылку linet://pair…") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedButton(enabled = !busy && uri.isNotBlank(), onClick = { pair(uri) }) { Text("Сопрячь") }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        }
    }
}

@Composable
private fun PeerCard(name: String, connected: Boolean, detail: String?) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var info by remember { mutableStateOf<String?>(null) }
    val pickFiles = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        LinetService.sendFiles(context, uris)
    }

    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(name, style = MaterialTheme.typography.titleMedium)
            Text(if (connected) "● Подключено" else "○ Не в сети")
            if (!connected && detail != null) {
                Text(detail, style = MaterialTheme.typography.bodySmall)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (connected) {
                    Button(onClick = { pickFiles.launch(arrayOf("*/*")) }) { Text("Файл") }
                    OutlinedButton(onClick = {
                        scope.launch {
                            info = io { Linet.client(context).ping() }
                                .fold({ "Ping: %.1f мс".format(it) }, { it.message })
                        }
                    }) { Text("Ping") }
                } else {
                    Button(onClick = { Linet.client(context).kick() }) { Text("Подключиться") }
                }
                OutlinedButton(onClick = {
                    scope.launch {
                        io { Linet.client(context).unpair() }
                        LinetService.stop(context)
                    }
                }) { Text("Отвязать") }
            }
            info?.let { Text(it) }
        }
    }
}

@Composable
private fun NotificationsCard() {
    val context = LocalContext.current
    var enabled by remember { mutableStateOf(NotifyListener.isEnabled(context)) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { enabled = NotifyListener.isEnabled(context) }
    if (enabled) return
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Уведомления на ноутбуке", style = MaterialTheme.typography.titleMedium)
            Text("Разреши Linet доступ к уведомлениям, чтобы видеть их на ноутбуке и отвечать оттуда.")
            Button(onClick = { NotifyListener.openSettings(context) }) { Text("Разрешить") }
        }
    }
}

@Composable
private fun PowerCard() {
    val context = LocalContext.current
    var ignoring by remember { mutableStateOf(Power.isIgnoringOptimizations(context)) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { ignoring = Power.isIgnoringOptimizations(context) }

    if (ignoring && !Power.isXiaomi) return
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Работа в фоне", style = MaterialTheme.typography.titleMedium)
            if (!ignoring) {
                Text("Без исключения из оптимизации батареи Android закроет Linet в фоне.")
                Button(onClick = { Power.requestIgnoreOptimizations(context) }) { Text("Снять ограничение") }
            }
            if (Power.isXiaomi) {
                Text(
                    "На Xiaomi ещё нужно: включить «Автозапуск», в «Контроле активности» выбрать " +
                        "«Нет ограничений» и закрепить Linet в списке недавних приложений (замок).",
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = { Power.openXiaomiAutostart(context) }) { Text("Автозапуск") }
                    OutlinedButton(onClick = { Power.openAppSettings(context) }) { Text("Настройки") }
                }
            }
        }
    }
}
