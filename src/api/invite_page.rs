use axum::response::Html;

/// Serve a self-contained HTML page for invite deep links.
///
/// When a user clicks `https://hub.bibliogenius.org/invite?d=BASE64`,
/// the browser loads this page. JavaScript decodes the payload,
/// shows the library name, and redirects to the native app via the
/// `bibliogenius://invite?d=...` custom URL scheme.
///
/// If the app is not installed, fallback instructions with download
/// links appear after a short delay.
pub async fn invite_page() -> Html<&'static str> {
    Html(INVITE_HTML)
}

const INVITE_HTML: &str = r##"<!DOCTYPE html>
<html lang="fr">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Invitation BiblioGenius</title>
    <meta name="description" content="Rejoignez une bibliotheque partagee sur BiblioGenius.">
    <style>
        *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif;
            background: linear-gradient(135deg, #f0fdf4 0%, #ecfeff 50%, #f0f9ff 100%);
            color: #1f2937;
            min-height: 100vh;
            display: flex;
            align-items: center;
            justify-content: center;
            padding: 1rem;
        }
        .card {
            background: #fff;
            border-radius: 1rem;
            box-shadow: 0 4px 24px rgba(0,0,0,0.08);
            padding: 2.5rem 2rem;
            max-width: 440px;
            width: 100%;
            text-align: center;
        }
        .icon { font-size: 3.5rem; margin-bottom: 1rem; }
        .label { font-size: 0.9rem; color: #6b7280; margin-bottom: 0.25rem; }
        .name {
            font-size: 1.5rem; font-weight: 700; color: #111827;
            margin-bottom: 0.5rem; word-break: break-word;
        }
        .text { font-size: 1rem; color: #374151; margin-bottom: 2rem; line-height: 1.5; }
        .btn {
            display: inline-flex; align-items: center; justify-content: center;
            gap: 0.5rem; background: #059669; color: #fff;
            padding: 0.875rem 2rem; border-radius: 0.5rem;
            font-size: 1.05rem; font-weight: 600;
            text-decoration: none; border: none; cursor: pointer;
            transition: background 0.2s; width: 100%;
        }
        .btn:hover { background: #047857; }
        .btn:focus-visible { box-shadow: 0 0 0 3px rgba(5,150,105,0.4); outline: none; }
        .fallback {
            display: none; margin-top: 1.5rem; padding-top: 1.5rem;
            border-top: 1px solid #e5e7eb;
        }
        .fallback.visible { display: block; }
        .fallback p { font-size: 0.9rem; color: #6b7280; margin-bottom: 1rem; line-height: 1.5; }
        .stores {
            display: flex; gap: 0.75rem; justify-content: center;
            flex-wrap: wrap; margin-bottom: 1rem;
        }
        .store-link {
            display: inline-flex; align-items: center; gap: 0.4rem;
            padding: 0.5rem 1rem; border: 1px solid #d1d5db; border-radius: 0.5rem;
            color: #374151; text-decoration: none; font-size: 0.85rem; font-weight: 500;
            transition: border-color 0.2s;
        }
        .store-link:hover { border-color: #059669; }
        .copy-btn {
            display: inline-flex; align-items: center; gap: 0.4rem;
            background: transparent; border: 1px solid #d1d5db; border-radius: 0.5rem;
            padding: 0.5rem 1rem; font-size: 0.85rem; color: #374151;
            cursor: pointer; transition: border-color 0.2s;
        }
        .copy-btn:hover { border-color: #059669; }
        .copy-btn.copied { border-color: #22c55e; color: #16a34a; }
        .error { color: #dc2626; font-size: 0.95rem; margin-top: 1rem; }
        .step-list {
            text-align: left; font-size: 0.85rem; color: #6b7280;
            margin: 0.75rem 0 1rem 1.5rem; line-height: 1.7;
        }
    </style>
</head>
<body>
    <div class="card" role="region" aria-label="Invitation de connexion">
        <div id="loading">
            <div class="icon" aria-hidden="true">&#128218;</div>
            <p style="color:#6b7280">Chargement de l'invitation...</p>
        </div>
        <div id="content" style="display:none">
            <div class="icon" aria-hidden="true">&#128218;</div>
            <p class="label">Invitation de</p>
            <p class="name" id="lib-name"></p>
            <p class="text">souhaite se connecter avec vous sur BiblioGenius pour partager des livres.</p>
            <button class="btn" id="open-btn" aria-label="Ouvrir dans l'application">
                &#128279; Ouvrir dans l'application
            </button>
            <div class="fallback" id="fallback">
                <p><strong>L'application ne s'est pas ouverte ?</strong></p>
                <ol class="step-list">
                    <li>Installez BiblioGenius depuis l'un des liens ci-dessous</li>
                    <li>Ouvrez l'application une premiere fois</li>
                    <li>Revenez sur cette page et cliquez de nouveau sur le bouton</li>
                </ol>
                <div class="stores">
                    <a href="https://github.com/SorellaLabs/BiblioGenius/releases"
                       class="store-link" target="_blank" rel="noopener"
                       aria-label="Telecharger depuis GitHub">
                        &#128187; GitHub Releases
                    </a>
                </div>
                <button class="copy-btn" id="copy-btn" aria-label="Copier le lien d'invitation">
                    &#128203; Copier le lien
                </button>
            </div>
        </div>
        <div id="error" style="display:none">
            <div class="icon" aria-hidden="true">&#128218;</div>
            <p class="error" id="error-text"></p>
        </div>
    </div>
<script>
(function() {
    'use strict';
    var loading = document.getElementById('loading');
    var content = document.getElementById('content');
    var errorEl = document.getElementById('error');
    var errorText = document.getElementById('error-text');
    var nameEl = document.getElementById('lib-name');
    var openBtn = document.getElementById('open-btn');
    var fallback = document.getElementById('fallback');
    var copyBtn = document.getElementById('copy-btn');

    // Extract payload: prefer ?d= query param (v4), fall back to #fragment (v3)
    var params = new URLSearchParams(window.location.search);
    var encoded = params.get('d');
    var source = 'query';
    if (!encoded && window.location.hash && window.location.hash.length > 1) {
        encoded = window.location.hash.substring(1);
        source = 'hash';
    }

    if (!encoded) {
        loading.style.display = 'none';
        errorEl.style.display = 'block';
        errorText.textContent = "Ce lien d'invitation est invalide ou incomplet.";
        return;
    }

    // Decode base64url
    var payload;
    try {
        var b64 = encoded.replace(/-/g, '+').replace(/_/g, '/');
        while (b64.length % 4) b64 += '=';
        var json = atob(b64);
        payload = JSON.parse(json);
    } catch (e) {
        loading.style.display = 'none';
        errorEl.style.display = 'block';
        errorText.textContent = "Impossible de lire les donnees d'invitation.";
        return;
    }

    // Extract name (v4 short key 'n' or v3 long key 'name')
    var libName = payload.n || payload.name || 'Bibliotheque';
    nameEl.textContent = libName;
    document.title = libName + ' - Invitation BiblioGenius';

    loading.style.display = 'none';
    content.style.display = 'block';

    // Build the custom-scheme deep link (always use ?d= for v4 compat)
    var deepLink = 'bibliogenius://invite?d=' + encoded;

    openBtn.addEventListener('click', function() {
        window.location.href = deepLink;
        // If the app opened, this page is now in the background.
        // If not, show fallback after a delay.
        setTimeout(function() {
            fallback.classList.add('visible');
        }, 2500);
    });

    copyBtn.addEventListener('click', function() {
        var link = window.location.href;
        if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(link).then(function() {
                copyBtn.classList.add('copied');
                copyBtn.innerHTML = '&#10003; Lien copie !';
                setTimeout(function() {
                    copyBtn.classList.remove('copied');
                    copyBtn.innerHTML = '&#128203; Copier le lien';
                }, 3000);
            });
        }
    });
})();
</script>
</body>
</html>"##;
