import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Минимальная конфигурация: один плагин (React), ничего лишнего.
// base: './' — чтобы собранный index.html корректно открывался из
// файловой системы (file://) внутри упакованного Electron-приложения,
// а не только с dev-сервера.
export default defineConfig({
  plugins: [react()],
  base: './',
});
