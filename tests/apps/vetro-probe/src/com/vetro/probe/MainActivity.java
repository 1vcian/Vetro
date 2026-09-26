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
 * App di prova per le analisi dall'esterno di Vetro (M7 TLS, M8 Binder).
 * All'avvio, in un thread: legge appunti e ANDROID_ID (transazioni Binder
 * sensibili) e fa una POST HTTPS con corpo JSON (testo in chiaro dagli
 * hook TLS). Vedi README.md.
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
        // M8: appunti (IClipboard.getPrimaryClip via Binder).
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
        // M8: ANDROID_ID (IContentProvider.call verso Settings via Binder).
        String androidId = "";
        try {
            androidId = Settings.Secure.getString(getContentResolver(), Settings.Secure.ANDROID_ID);
        } catch (Throwable t) {
            Log.w(TAG, "android_id", t);
        }
        Log.i(TAG, "appunti=" + clip + " android_id=" + androidId);

        // M7: POST HTTPS con corpo JSON (chiaro dagli hook TLS).
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
     * Per la sola prova: accetta ogni certificato, così la connessione si
     * completa contro l'endpoint TLS della sinkhole (il chiaro viene dagli
     * hook, non dai record). In produzione userebbe la CA di sviluppo di
     * Vetro nel trust store.
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
