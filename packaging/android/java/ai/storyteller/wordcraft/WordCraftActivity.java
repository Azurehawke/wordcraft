package ai.storyteller.wordcraft;

import android.app.NativeActivity;
import android.content.Intent;

/**
 * WordCraft's activity: Android's NativeActivity (which loads libwordcraft_android.so) plus
 * what Rust can't do through JNI alone, because it means overriding an Activity method.
 *
 * The activity is single-task, so a document opened from another app while WordCraft runs
 * arrives in onNewIntent instead of starting a second copy; keeping it as the activity's intent
 * lets the Rust side find it (apps/wordcraft-android/src/incoming.rs).
 */
public class WordCraftActivity extends NativeActivity {
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
    }
}
