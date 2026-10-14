package ai.storyteller.wordcraft;

import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.Intent;
import android.content.pm.PackageInfo;
import android.content.pm.PackageInstaller;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.provider.Settings;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;

/**
 * WordCraft's activity: Android's NativeActivity (which loads libwordcraft_android.so) plus
 * what Rust can't do through JNI alone, or does more simply in Java.
 *
 * - The activity is single-task, so a document opened from another app while WordCraft runs
 *   arrives in onNewIntent instead of starting a second copy; keeping it as the activity's
 *   intent lets the Rust side find it (apps/wordcraft-android/src/incoming.rs).
 * - Updates (apps/wordcraft-android/src/updater.rs): HTTP for the release listing and the APK
 *   download, and installing the downloaded APK through PackageInstaller. Android answers an
 *   install with an intent back to this activity: either "ask the user" (the system's install
 *   confirmation, started here) or the outcome, kept for the Rust side to collect.
 */
public class WordCraftActivity extends NativeActivity {
    private static final String INSTALL_STATUS = "ai.storyteller.wordcraft.INSTALL_STATUS";
    private static final String USER_AGENT = "WordCraft-Android-updater";

    private volatile long downloaded;
    private volatile long downloadTotal;
    private volatile String installStatus;

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (intent != null && INSTALL_STATUS.equals(intent.getAction())) {
            installAnswered(intent);
            return;
        }
        setIntent(intent);
    }

    /** This app's versionCode (the CI build number). */
    public long wcVersionCode() throws PackageManager.NameNotFoundException {
        PackageInfo info = getPackageManager().getPackageInfo(getPackageName(), 0);
        return info.getLongVersionCode();
    }

    private HttpURLConnection open(String url) throws IOException {
        HttpURLConnection c = (HttpURLConnection) new URL(url).openConnection();
        c.setRequestProperty("User-Agent", USER_AGENT);
        c.setConnectTimeout(15000);
        c.setReadTimeout(30000);
        c.setInstanceFollowRedirects(true);
        int code = c.getResponseCode();
        if (code / 100 != 2) {
            c.disconnect();
            throw new IOException("HTTP " + code + " from " + new URL(url).getHost());
        }
        return c;
    }

    /** GET `url` as text (the GitHub release listing). Blocking: call off the UI thread. */
    public String wcHttpGet(String url) throws IOException {
        HttpURLConnection c = open(url);
        c.setRequestProperty("Accept", "application/vnd.github+json");
        try (InputStream in = c.getInputStream()) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[16384];
            int n;
            while ((n = in.read(buf)) >= 0) {
                out.write(buf, 0, n);
                if (out.size() > (8 << 20)) {
                    throw new IOException("answer too large");
                }
            }
            return out.toString("UTF-8");
        } finally {
            c.disconnect();
        }
    }

    private File updateFile() {
        return new File(getCacheDir(), "update.apk");
    }

    /** Download `url` to the app's cache as the update APK. Blocking: call off the UI thread. */
    public void wcDownload(String url) throws IOException {
        downloaded = 0;
        downloadTotal = 0;
        File tmp = new File(getCacheDir(), "update.apk.part");
        HttpURLConnection c = open(url);
        downloadTotal = Math.max(0, c.getContentLengthLong());
        try (InputStream in = c.getInputStream(); OutputStream out = new FileOutputStream(tmp)) {
            byte[] buf = new byte[65536];
            int n;
            while ((n = in.read(buf)) >= 0) {
                out.write(buf, 0, n);
                downloaded += n;
            }
        } finally {
            c.disconnect();
        }
        File apk = updateFile();
        apk.delete();
        if (!tmp.renameTo(apk)) {
            throw new IOException("couldn't keep the download");
        }
    }

    /** Bytes downloaded so far. */
    public long wcDownloaded() {
        return downloaded;
    }

    /** The download's size (0 when the server didn't say). */
    public long wcDownloadTotal() {
        return downloadTotal;
    }

    /**
     * Install the downloaded APK. Returns false (and opens the setting) when WordCraft may not
     * install apps yet; Android then asks the user to confirm the install itself.
     */
    public boolean wcInstall() throws IOException {
        if (!getPackageManager().canRequestPackageInstalls()) {
            Intent s = new Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:" + getPackageName()));
            s.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            startActivity(s);
            return false;
        }
        installStatus = null;
        File apk = updateFile();
        PackageInstaller installer = getPackageManager().getPackageInstaller();
        PackageInstaller.SessionParams params = new PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL);
        params.setAppPackageName(getPackageName());
        params.setSize(apk.length());
        int id = installer.createSession(params);
        PackageInstaller.Session session = installer.openSession(id);
        try {
            try (InputStream in = new FileInputStream(apk); OutputStream out = session.openWrite("wordcraft.apk", 0, apk.length())) {
                byte[] buf = new byte[65536];
                int n;
                while ((n = in.read(buf)) >= 0) {
                    out.write(buf, 0, n);
                }
                session.fsync(out);
            }
            Intent status = new Intent(this, WordCraftActivity.class).setAction(INSTALL_STATUS);
            // Mutable: the installer adds the outcome to the intent.
            PendingIntent pending = PendingIntent.getActivity(this, id, status,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_MUTABLE);
            session.commit(pending.getIntentSender());
        } catch (IOException | RuntimeException e) {
            session.abandon();
            throw e;
        } finally {
            session.close();
        }
        return true;
    }

    private void installAnswered(Intent intent) {
        int status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            Intent confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT);
            if (confirm != null) {
                confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                startActivity(confirm);
                return;
            }
        }
        if (status == PackageInstaller.STATUS_SUCCESS) {
            installStatus = "ok";
        } else {
            String message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE);
            installStatus = (status == PackageInstaller.STATUS_FAILURE_ABORTED ? "cancelled: " : "failed: ")
                    + (message == null ? ("status " + status) : message);
        }
        updateFile().delete();
    }

    /** The install's outcome once Android reports it ("ok", "cancelled: …", "failed: …"), once. */
    public String wcTakeInstallStatus() {
        String s = installStatus;
        installStatus = null;
        return s;
    }
}
