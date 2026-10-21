import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './style.css';

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(
    <StrictMode>
      <h1>Yalper viewer</h1>
    </StrictMode>,
  );
}
