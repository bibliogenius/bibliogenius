// Simple i18n utility for BiblioGenius
class I18n {
    constructor() {
        this.currentLang = localStorage.getItem('lang') || 'en';
        this.translations = {};
    }

    async load(lang) {
        if (this.translations[lang]) {
            this.currentLang = lang;
            return;
        }

        try {
            const response = await fetch(`/i18n/${lang}.json`);
            this.translations[lang] = await response.json();
            this.currentLang = lang;
            localStorage.setItem('lang', lang);
        } catch (error) {
            console.error(`Failed to load language ${lang}:`, error);
            // Fallback to English
            if (lang !== 'en') {
                await this.load('en');
            }
        }
    }

    t(key) {
        const keys = key.split('.');
        let value = this.translations[this.currentLang];

        for (const k of keys) {
            if (value && typeof value === 'object') {
                value = value[k];
            } else {
                return key; // Return key if translation not found
            }
        }

        return value || key;
    }

    applyTranslations() {
        document.querySelectorAll('[data-i18n]').forEach(element => {
            const key = element.getAttribute('data-i18n');
            const translated = this.t(key);

            if (element.tagName === 'INPUT' || element.tagName === 'TEXTAREA') {
                element.placeholder = translated;
            } else {
                element.textContent = translated;
            }
        });
    }

    async setLanguage(lang) {
        await this.load(lang);
        this.applyTranslations();
        // Dispatch event for other components
        window.dispatchEvent(new CustomEvent('languageChanged', { detail: { lang } }));
    }
}

const i18n = new I18n();
