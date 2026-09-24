// Shared by templates/log_viewer.html and templates/apps.html.
// navigator.clipboard only exists in a secure context (https, or localhost) -- this app
// may well be reached over plain http on an internal server (see COOKIE_SECURE), where
// that API is undefined. Falls back to the classic hidden-textarea + execCommand trick.
async function copyToClipboard(text) {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path
    }
  }
  try {
    const el = document.createElement('textarea');
    el.value = text;
    el.style.position = 'fixed';
    el.style.opacity = '0';
    document.body.appendChild(el);
    el.select();
    document.execCommand('copy');
    document.body.removeChild(el);
    return true;
  } catch {
    return false;
  }
}
