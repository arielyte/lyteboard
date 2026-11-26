package app.lyteboard.android

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import app.lyteboard.core.HistoryItem
import app.lyteboard.core.Kind
import java.io.File

internal data class Retention(val entries: Int = 100, val mib: Int = 100, val days: Int = 7) {
    init {
        require(entries in 1..500 && mib in 1..256 && days in 1..365)
    }
}

internal data class HistoryRow(
    val id: Long,
    val peer: String,
    val kind: Kind,
    val sent: Boolean,
    val timestamp: Long,
    val size: Int,
)

internal class LocalHistory(context: Context, filename: String = "clipboard-history.db") :
    SQLiteOpenHelper(context, File(context.noBackupFilesDir, filename).path, null, 1) {
    override fun onConfigure(db: SQLiteDatabase) {
        db.setForeignKeyConstraintsEnabled(true)
        db.rawQuery("PRAGMA secure_delete = ON", null).use { it.moveToFirst() }
        if (db.version == 0) db.rawQuery("PRAGMA auto_vacuum = FULL", null).use { it.moveToFirst() }
    }

    override fun onCreate(db: SQLiteDatabase) {
        db.execSQL(
            """CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                peer TEXT NOT NULL,
                kind TEXT NOT NULL,
                sent INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                size INTEGER NOT NULL
            )""",
        )
        db.execSQL(
            """CREATE TABLE chunks (
                item INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                part INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (item, part)
            ) WITHOUT ROWID""",
        )
    }

    override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) {
        error("This history database format is unsupported.")
    }

    fun add(item: HistoryItem, limits: Retention) {
        val bytes = item.bytes
        if (bytes.size > limits.mib * 1024 * 1024) return
        transaction { db ->
            val id = db.insertOrThrow("items", null, ContentValues().apply {
                put("peer", item.deviceId)
                put("kind", item.kind.name)
                put("sent", item.sent)
                put("timestamp", item.timestamp.toLong())
                put("size", bytes.size)
            })
            var offset = 0
            var part = 0
            while (offset < bytes.size) {
                val end = minOf(offset + 256 * 1024, bytes.size)
                val chunk = bytes.copyOfRange(offset, end)
                try {
                    db.insertOrThrow("chunks", null, ContentValues().apply {
                        put("item", id)
                        put("part", part++)
                        put("data", chunk)
                    })
                } finally {
                    chunk.fill(0)
                }
                offset = end
            }
            prune(db, limits)
        }
    }

    fun cleanup(limits: Retention) = transaction { prune(it, limits) }

    private fun prune(db: SQLiteDatabase, limits: Retention) {
        val cutoff = System.currentTimeMillis() / 1000 - limits.days * 86_400L
        db.delete("items", "timestamp < ?", arrayOf(cutoff.toString()))
        db.execSQL("DELETE FROM items WHERE id IN (SELECT id FROM items ORDER BY id DESC LIMIT -1 OFFSET ?)",
            arrayOf(limits.entries))
        var remaining = limits.mib * 1024 * 1024
        var firstExpired: Long? = null
        db.rawQuery("SELECT id, size FROM items ORDER BY id DESC", null).use { rows ->
            while (rows.moveToNext()) {
                remaining -= rows.getInt(1)
                if (remaining < 0) {
                    firstExpired = rows.getLong(0)
                    break
                }
            }
        }
        firstExpired?.let { db.delete("items", "id <= ?", arrayOf(it.toString())) }
    }

    fun list(): List<HistoryRow> = readableDatabase.rawQuery(
        "SELECT id, peer, kind, sent, timestamp, size FROM items ORDER BY id DESC LIMIT 500", null,
    ).use { rows ->
        buildList {
            while (rows.moveToNext()) add(HistoryRow(
                rows.getLong(0), rows.getString(1), Kind.valueOf(rows.getString(2)),
                rows.getInt(3) != 0, rows.getLong(4), rows.getInt(5),
            ))
        }
    }

    fun load(row: HistoryRow): Content {
        val limit = if (row.kind == Kind.TEXT) TEXT_LIMIT else IMAGE_LIMIT
        require(row.size in 1..limit)
        val result = ByteArray(row.size)
        var offset = 0
        readableDatabase.rawQuery(
            "SELECT data FROM chunks WHERE item = ? ORDER BY part", arrayOf(row.id.toString()),
        ).use { rows ->
            while (rows.moveToNext()) {
                val bytes = rows.getBlob(0)
                check(bytes.size <= result.size - offset) { "History item is damaged." }
                bytes.copyInto(result, offset)
                offset += bytes.size
            }
        }
        check(offset == result.size) { "History item is unavailable or damaged." }
        return Content(row.kind, result)
    }

    fun delete(id: Long) = transaction { it.delete("items", "id = ?", arrayOf(id.toString())) }
    fun clear() = transaction { it.delete("items", null, null) }

    @Synchronized
    private fun transaction(action: (SQLiteDatabase) -> Unit) {
        val db = writableDatabase
        db.beginTransaction()
        try {
            action(db)
            db.setTransactionSuccessful()
        } finally {
            db.endTransaction()
        }
    }
}
