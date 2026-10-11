window.addEventListener('message', function(e) {
  if (e.origin !== 'https://board.battlesnake.com') return;
  const evt = e.data;
  if (evt?.event === 'RESIZE') {
    (document.getElementById('board-viewer-container') as HTMLElement | null)?.style
      .setProperty('aspect-ratio', evt.data.width + ' / ' + evt.data.height);
  }
});
