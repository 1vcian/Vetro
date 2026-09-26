// Vetro's test app (M5/M6): a screen of one colour that changes at every
// touch. The tests install it with the page's ADB client, open it with
// `am start` and check the scanout pixels before and after a touch
// (tests/web/android.mjs, tests/web/android-chrome.mjs).
//
// Colours: blue (0xff1565c0) at start, then orange and blue alternating; the
// number of touches above the centre. Every touch also goes to the log
// ("vetro-tocco: tocco N") and to the app's files/tocchi file.
package it.vetro.tocco;

import android.app.Activity;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.os.Bundle;
import android.util.Log;
import android.view.MotionEvent;
import android.view.View;
import java.io.FileOutputStream;

public class Main extends Activity {
    static final int BLU = 0xff1565c0;
    static final int ARANCIONE = 0xffef6c00;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        setContentView(new Schermo(this));
        Log.i("vetro-tocco", "avviata");
    }

    static class Schermo extends View {
        int tocchi = 0;
        final Paint testo = new Paint(Paint.ANTI_ALIAS_FLAG);

        Schermo(Context c) {
            super(c);
            testo.setColor(0xffffffff);
            testo.setTextSize(96);
            testo.setTextAlign(Paint.Align.CENTER);
        }

        @Override
        protected void onDraw(Canvas c) {
            c.drawColor(tocchi % 2 == 0 ? BLU : ARANCIONE);
            // The number sits above the centre: the central pixel keeps the background colour.
            c.drawText(Integer.toString(tocchi), getWidth() / 2f, getHeight() / 4f, testo);
        }

        @Override
        public boolean onTouchEvent(MotionEvent e) {
            if (e.getActionMasked() == MotionEvent.ACTION_DOWN) {
                tocchi++;
                Log.i("vetro-tocco", "tocco " + tocchi);
                try (FileOutputStream f = getContext().openFileOutput("tocchi", Context.MODE_PRIVATE)) {
                    f.write(Integer.toString(tocchi).getBytes("UTF-8"));
                } catch (Exception ex) {
                    Log.w("vetro-tocco", "file", ex);
                }
                invalidate();
            }
            return true;
        }
    }
}
