import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';

// Стандартный монтаж React 18 — без лишней обвязки (роутера,
// провайдеров состояния и т.п.). Они появятся тогда, когда реально
// понадобятся, не заранее "на всякий случай".
ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
