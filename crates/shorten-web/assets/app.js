document.addEventListener('click', async (event) => {
  const button = event.target.closest('[data-copy]');
  if (!button) return;
  try {
    await navigator.clipboard.writeText(button.dataset.copy);
    const original = button.textContent;
    button.textContent = 'Copied';
    window.setTimeout(() => { button.textContent = original; }, 1400);
  } catch {
    const message = document.querySelector('[role="status"]');
    if (message) message.textContent = 'Copy failed. Select the short URL and copy it manually.';
  }
});

document.addEventListener('htmx:afterSwap', () => {
  window.requestAnimationFrame(() => {
    const invalid = document.querySelector('[aria-invalid="true"]');
    if (invalid) invalid.focus();
  });
});
