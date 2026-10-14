//! Documents other apps send to WordCraft ("Open with", or WordCraft as the default app for Word
//! documents). From PdfCraft's Android port.
//!
//! The manifest offers WordCraft for `VIEW` and `EDIT` of Word, OpenDocument, RTF, plain-text and
//! Markdown documents. The activity's intent says which file: at start-up the one that launched
//! the app, later the one `WordCraftActivity.onNewIntent` stored (the activity is single-task, so
//! a second document reaches the running app). Each intent is opened once: it is marked handled
//! by changing its action.
//!
//! Most apps send a `content://` URI. When it names a file in shared storage (Files, My Files,
//! Downloads) WordCraft opens that file by path, so Save writes it in place (with All files
//! access). Otherwise (an attachment, a Drive file) it reads the bytes, and the app keeps them as
//! a file of its own to open (`tablet.rs`).

use jni::JNIEnv;
use jni::objects::{JObject, JString, JValue};

/// Larger files are refused rather than read into memory.
const MAX_BYTES: usize = 1 << 30;
const HANDLED: &str = "ai.storyteller.wordcraft.HANDLED";

#[derive(Debug)]
pub enum Incoming {
    /// A file WordCraft can read (and save) by path.
    Path(String),
    /// Only the content: its name and bytes.
    Bytes { name: String, bytes: Vec<u8> },
    /// Something was sent but couldn't be read.
    Failed(String),
}

/// The document the activity was asked to open, if it hasn't been opened yet.
pub fn take() -> Option<Incoming> {
    match crate::file_access::with_activity("Opening a file from another app", take_intent) {
        Ok(found) => found,
        Err(e) => Some(Incoming::Failed(e)),
    }
}

fn take_intent(env: &mut JNIEnv, activity: &JObject) -> jni::errors::Result<Option<Incoming>> {
    let intent = env.call_method(activity, "getIntent", "()Landroid/content/Intent;", &[])?.l()?;
    if intent.is_null() {
        return Ok(None);
    }
    let action = text_of(env, &intent, "getAction")?;
    if !matches!(action.as_deref(), Some("android.intent.action.VIEW" | "android.intent.action.EDIT")) {
        return Ok(None);
    }
    // Opened once, whatever happens next.
    let handled = env.new_string(HANDLED)?;
    env.call_method(&intent, "setAction", "(Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&handled)])?;
    let uri = env.call_method(&intent, "getData", "()Landroid/net/Uri;", &[])?.l()?;
    if uri.is_null() {
        return Ok(None);
    }
    let text = text_of(env, &uri, "toString")?.unwrap_or_default();
    log::info!("asked to open {text}");
    let scheme = text_of(env, &uri, "getScheme")?;
    if scheme.as_deref() == Some("file") {
        let path = text_of(env, &uri, "getPath")?;
        return Ok(Some(path.map_or_else(|| Incoming::Failed(format!("No file in {text}")), Incoming::Path)));
    }
    let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
    if let Some(path) = shared_storage_path(env, &resolver, &uri)
        && std::fs::File::open(&path).is_ok()
    {
        log::info!("opening it as {path}");
        return Ok(Some(Incoming::Path(path)));
    }
    let name = column(env, &resolver, &uri, "_display_name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Document.docx".to_string());
    Ok(Some(match read_all(env, &resolver, &uri) {
        Ok(bytes) => Incoming::Bytes { name, bytes },
        Err(e) => Incoming::Failed(format!("Couldn't read {name}: {e}")),
    }))
}

/// `obj.method()` for a method returning a String (None for null).
pub(crate) fn text_of(env: &mut JNIEnv, obj: &JObject, method: &str) -> jni::errors::Result<Option<String>> {
    let s = env.call_method(obj, method, "()Ljava/lang/String;", &[])?.l()?;
    string_of(env, s)
}

pub(crate) fn string_of(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<Option<String>> {
    if obj.is_null() {
        return Ok(None);
    }
    Ok(Some(env.get_string(&JString::from(obj))?.into()))
}

/// Run a Java call whose failure only means "not this way": a thrown exception is cleared.
pub(crate) fn quietly<'local, R>(env: &mut JNIEnv<'local>, f: impl FnOnce(&mut JNIEnv<'local>) -> jni::errors::Result<R>) -> Option<R> {
    let result = f(env);
    if crate::file_access::java_exception(env).is_some() {
        return None;
    }
    result.ok()
}

/// The file in shared storage a content URI stands for, when it says: a document from the
/// external-storage provider ("primary:Download/a.docx"), a provider's `_data` column, or a
/// `/storage/…` path inside the URI.
fn shared_storage_path(env: &mut JNIEnv, resolver: &JObject, uri: &JObject) -> Option<String> {
    let authority = quietly(env, |env| text_of(env, uri, "getAuthority")).flatten();
    if matches!(authority.as_deref(), Some("com.android.externalstorage.documents" | "com.android.providers.downloads.documents"))
        && let Some(id) = quietly(env, |env| {
            let id = env
                .call_static_method(
                    "android/provider/DocumentsContract",
                    "getDocumentId",
                    "(Landroid/net/Uri;)Ljava/lang/String;",
                    &[JValue::Object(uri)],
                )?
                .l()?;
            string_of(env, id)
        })
        .flatten()
        && let Some(path) = external_storage_path(&id)
        && std::path::Path::new(&path).is_file()
    {
        return Some(path);
    }
    if let Some(path) = column(env, resolver, uri, "_data").filter(|p| p.starts_with('/')) {
        return Some(path);
    }
    let decoded = quietly(env, |env| text_of(env, uri, "getPath")).flatten()?;
    decoded.find("/storage/").and_then(|i| decoded.get(i..)).map(str::to_string)
}

/// "primary:Download/a.docx" → /storage/emulated/0/Download/a.docx; "1A2B-3C4D:x.docx" (an SD card
/// or USB drive) → /storage/1A2B-3C4D/x.docx; the Downloads provider's "raw:/storage/…" → its path.
fn external_storage_path(id: &str) -> Option<String> {
    let (volume, rest) = id.split_once(':')?;
    if rest.is_empty() || rest.split('/').any(|part| part == "..") {
        return None;
    }
    Some(match volume {
        "raw" if rest.starts_with('/') => rest.to_string(),
        "primary" => format!("/storage/emulated/0/{rest}"),
        "home" => format!("/storage/emulated/0/Documents/{rest}"),
        // A volume's id: hex digits around a dash.
        v if v.contains('-') && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-') => format!("/storage/{v}/{rest}"),
        _ => return None,
    })
}

/// One string column of the URI's row (`_display_name`, `_data`), when the provider has it.
pub(crate) fn column(env: &mut JNIEnv, resolver: &JObject, uri: &JObject, name: &str) -> Option<String> {
    let cursor = quietly(env, |env| {
        let col = env.new_string(name)?;
        let projection = env.new_object_array(1, "java/lang/String", &col)?;
        let none = JObject::null();
        env.call_method(
            resolver,
            "query",
            "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
            &[JValue::Object(uri), JValue::Object(&projection), JValue::Object(&none), JValue::Object(&none), JValue::Object(&none)],
        )?
        .l()
    })?;
    if cursor.is_null() {
        return None;
    }
    let value = quietly(env, |env| {
        if !env.call_method(&cursor, "moveToFirst", "()Z", &[])?.z()? {
            return Ok(None);
        }
        let col = env.new_string(name)?;
        let index = env.call_method(&cursor, "getColumnIndex", "(Ljava/lang/String;)I", &[JValue::Object(&col)])?.i()?;
        if index < 0 {
            return Ok(None);
        }
        let s = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[JValue::Int(index)])?.l()?;
        string_of(env, s)
    })
    .flatten();
    let _ = quietly(env, |env| env.call_method(&cursor, "close", "()V", &[]));
    value
}

/// The whole content behind the URI, through the content resolver.
pub(crate) fn read_all(env: &mut JNIEnv, resolver: &JObject, uri: &JObject) -> Result<Vec<u8>, String> {
    let stream =
        quietly(env, |env| env.call_method(resolver, "openInputStream", "(Landroid/net/Uri;)Ljava/io/InputStream;", &[JValue::Object(uri)])?.l())
            .filter(|s| !s.is_null())
            .ok_or("the app that sent it didn't let WordCraft read it")?;
    let read = (|| -> Result<Vec<u8>, String> {
        let chunk = quietly(env, |env| env.new_byte_array(64 * 1024)).ok_or("out of memory")?;
        let mut bytes = Vec::new();
        let mut buf = vec![0i8; 64 * 1024];
        loop {
            let n = quietly(env, |env| env.call_method(&stream, "read", "([B)I", &[JValue::Object(&chunk)])?.i()).ok_or("reading failed")?;
            if n < 0 {
                return Ok(bytes);
            }
            let n = usize::try_from(n).unwrap_or(0).min(buf.len());
            let part = buf.get_mut(..n).ok_or("reading failed")?;
            quietly(env, |env| env.get_byte_array_region(&chunk, 0, part)).ok_or("reading failed")?;
            if bytes.len().saturating_add(n) > MAX_BYTES {
                return Err("the file is larger than 1 GB".to_string());
            }
            bytes.extend(part.iter().map(|b| *b as u8));
        }
    })();
    let _ = quietly(env, |env| env.call_method(&stream, "close", "()V", &[]));
    read
}

#[cfg(test)]
mod tests {
    use super::external_storage_path;

    #[test]
    fn external_storage_document_ids_become_paths() {
        assert_eq!(external_storage_path("primary:Download/a.docx").as_deref(), Some("/storage/emulated/0/Download/a.docx"));
        assert_eq!(external_storage_path("1A2B-3C4D:x.docx").as_deref(), Some("/storage/1A2B-3C4D/x.docx"));
        assert_eq!(external_storage_path("primary:../etc/x"), None);
        assert_eq!(external_storage_path("raw:/storage/emulated/0/Download/a.docx").as_deref(), Some("/storage/emulated/0/Download/a.docx"));
        // The Downloads provider's MediaStore ids aren't paths.
        assert_eq!(external_storage_path("msf:123"), None);
        assert_eq!(external_storage_path("primary:"), None);
    }
}
