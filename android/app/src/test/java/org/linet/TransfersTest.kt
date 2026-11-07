package org.linet

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.content.pm.ProviderInfo
import android.database.Cursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowContentResolver
import java.io.File
import java.io.IOException
import java.io.OutputStream

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29], manifest = Config.NONE)
class TransfersTest {
    private lateinit var context: Context
    private lateinit var source: File
    private lateinit var destination: File
    private lateinit var provider: DownloadsProvider

    @Before
    fun prepare() {
        context = RuntimeEnvironment.getApplication()
        source = File(context.filesDir, "incoming/test.bin").apply {
            parentFile!!.mkdirs()
            writeText("received bytes")
        }
        destination = File(context.cacheDir, "download.bin")
        provider = DownloadsProvider(destination)
        provider.attachInfo(context, ProviderInfo().apply { authority = "media" })
        ShadowContentResolver.registerProviderInternal("media", provider)
    }

    @Test
    fun successfulSaveDeletesSourceOnlyAfterPublishing() {
        val uri = Transfers.saveToDownloads(context, source, "test.bin")
        assertEquals(provider.uri, uri)
        assertEquals("received bytes", destination.readText())
        assertTrue(provider.published)
        assertFalse(source.exists())
        assertFalse(provider.deleted)
    }

    @Test
    fun outputFailureKeepsReceivedFileAndRemovesPendingEntry() {
        shadowOf(context.contentResolver).registerOutputStream(provider.uri, object : OutputStream() {
            private var written = 0
            override fun write(byte: Int) {
                if (++written > 4) throw IOException("No space left")
            }
        })
        assertThrows(IOException::class.java) { Transfers.saveToDownloads(context, source, "test.bin") }
        assertEquals("received bytes", source.readText())
        assertTrue(provider.deleted)
    }

    @Test
    fun publicationFailureKeepsReceivedFile() {
        provider.updateCount = 0
        assertThrows(IllegalStateException::class.java) { Transfers.saveToDownloads(context, source, "test.bin") }
        assertEquals("received bytes", source.readText())
        assertTrue(provider.deleted)
    }

    @Test
    fun nullInsertIsAnErrorAndKeepsReceivedFile() {
        provider.failInsert = true
        assertThrows(IllegalStateException::class.java) { Transfers.saveToDownloads(context, source, "test.bin") }
        assertEquals("received bytes", source.readText())
        assertFalse(provider.deleted)
    }

    @Test
    fun cleanupFailureDoesNotHideOriginalFailureOrDeleteSource() {
        shadowOf(context.contentResolver).registerOutputStream(provider.uri, object : OutputStream() {
            override fun write(byte: Int) { throw IOException("No space left") }
        })
        provider.failDelete = true
        val error = assertThrows(IOException::class.java) {
            Transfers.saveToDownloads(context, source, "test.bin")
        }
        assertEquals(1, error.suppressed.size)
        assertEquals("received bytes", source.readText())
    }

    class DownloadsProvider(private val destination: File) : ContentProvider() {
        val uri: Uri = Uri.parse("content://media/external/downloads/1")
        var failInsert = false
        var failDelete = false
        var updateCount = 1
        var published = false
        var deleted = false

        override fun onCreate() = true
        override fun getType(uri: Uri) = "application/octet-stream"
        override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor? = null
        override fun insert(uri: Uri, values: ContentValues?): Uri? = if (failInsert) null else this.uri
        override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
            return ParcelFileDescriptor.open(destination, ParcelFileDescriptor.MODE_CREATE or ParcelFileDescriptor.MODE_READ_WRITE or ParcelFileDescriptor.MODE_TRUNCATE)
        }
        override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int {
            published = updateCount == 1
            return updateCount
        }
        override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int {
            if (failDelete) throw IllegalStateException("cleanup failed")
            deleted = true
            destination.delete()
            return 1
        }
    }
}
