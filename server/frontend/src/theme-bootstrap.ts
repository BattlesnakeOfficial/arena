import { resolvedTheme } from './theme-preferences';

document.documentElement.setAttribute('data-app-theme', resolvedTheme(document.documentElement));
