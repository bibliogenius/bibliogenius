class ThemeManager {
    constructor() {
        this.themes = ['default', 'dark', 'cozy', 'professional'];
    }

    apply(theme) {
        if (!this.themes.includes(theme)) return;

        // Remove existing theme classes
        document.body.classList.remove(...this.themes.map(t => `theme-${t}`));

        // Add new theme class (if not default, though default can be explicit too)
        if (theme !== 'default') {
            document.body.classList.add(`theme-${theme}`);
        }

        localStorage.setItem('theme', theme);
    }

    async load() {
        // Try local storage first for immediate effect
        const localTheme = localStorage.getItem('theme');
        if (localTheme) {
            this.apply(localTheme);
        }

        // Then sync with server config
        try {
            const response = await fetch('/api/config');
            if (response.ok) {
                const config = await response.json();
                if (config.theme && config.theme !== localTheme) {
                    this.apply(config.theme);
                }
            }
        } catch (e) {
            console.error("Failed to load theme config", e);
        }
    }
}

const themeManager = new ThemeManager();
document.addEventListener('DOMContentLoaded', () => themeManager.load());
