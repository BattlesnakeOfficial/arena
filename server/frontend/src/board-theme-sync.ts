(() => {
  const frame = document.getElementById('board-viewer') as HTMLIFrameElement | null;
  const theme = document.documentElement.getAttribute('data-app-theme');
  if (!frame || !theme) return;
  if (frame.src.indexOf('theme=' + theme) === -1) {
    frame.src = frame.src.replace(/theme=[a-z]+/, 'theme=' + theme);
  }
})();
