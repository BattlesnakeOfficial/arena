(function() {
  var root = document.getElementById('game-live-state');
  if (!root) return;
  var active = root.dataset.status;
  if (active !== 'waiting' && active !== 'running') return;
  var key = 'arena-game-poll-' + root.dataset.gameId;
  var used = 0, failures = 0, stopped = false, phase = 'status';
  let timer: ReturnType<typeof setTimeout> | null = null;
  var inFlight = false, epoch = 0;
  let controller: AbortController | null = null;
  var regions = ['game-status-region', 'game-outcome-region', 'game-actions-region', 'game-results-region', 'game-metadata-region'];
  try { used = Number(sessionStorage.getItem(key)) || 0; } catch (_) {}
  function store() { try { sessionStorage.setItem(key, String(used)); } catch (_) {} }
  function clearBudget() { try { sessionStorage.removeItem(key); } catch (_) {} }
  function revealRefresh() { var el = document.getElementById('game-manual-refresh'); if (el) el.hidden = false; }
  function stop(done: boolean) {
    stopped = true;
    if (timer) clearTimeout(timer);
    timer = null;
    if (controller) controller.abort();
    epoch += 1;
    inFlight = false;
    controller = null;
    if (done) clearBudget();
  }
  function delay(ok: boolean) { if (ok) failures = 0; else failures += 1; return ok ? 2000 : Math.min(30000, 2000 * Math.pow(2, failures)); }
  function schedule(ms: number) { if (!stopped && !timer) timer = setTimeout(run, ms); }
  function attempt<T>(url: string, parse: (response: Response) => Promise<T>): Promise<T> | null {
    if (used >= 600) { stop(false); revealRefresh(); return null; }
    used += 1; store();
    var requestEpoch = epoch;
    var current = new AbortController();
    var timedOut = false;
    var deadline = setTimeout(function() {
      timedOut = true;
      current.abort();
    }, 15000);
    controller = current;
    inFlight = true;
    return fetch(url, { credentials: 'same-origin', signal: current.signal })
      .then(function(response) {
        if (stopped || requestEpoch !== epoch) throw new DOMException('stale request', 'AbortError');
        if (!response.ok) throw new Error('request failed');
        return parse(response);
      })
      .then(function(value) {
        if (stopped || requestEpoch !== epoch) throw new DOMException('stale request', 'AbortError');
        return value;
      })
      .catch(function(error) {
        if (timedOut && !stopped && requestEpoch === epoch) throw new Error('request timed out');
        throw error;
      })
      .finally(function() {
        clearTimeout(deadline);
        if (controller === current && requestEpoch === epoch) {
          inFlight = false;
          controller = null;
        }
      });
  }
  function retry() { schedule(delay(false)); }
  function refreshTerminal() {
    phase = 'terminal';
    var request = attempt(window.location.href, function(response: Response) { return response.text(); });
    if (!request) return;
    request.then(function(text) {
      var source = new DOMParser().parseFromString(text, 'text/html');
      var pairs = regions.map(function(id) {
        var from = source.querySelectorAll('#' + id);
        var to = document.querySelectorAll('#' + id);
        if (from.length !== 1 || to.length !== 1) throw new Error('invalid refresh regions');
        return [to[0], from[0]];
      });
      pairs.forEach(function(pair) { pair[0].innerHTML = pair[1].innerHTML; });
      stop(true);
    }).catch(function(error) {
      if (stopped || error.name === 'AbortError') return;
      retry();
    });
  }
  function run() {
    timer = null;
    if (stopped || inFlight) return;
    if (document.hidden) { schedule(1000); return; }
    if (phase === 'terminal') { refreshTerminal(); return; }
    var request = attempt('/api/games/' + root!.dataset.gameId, function(response: Response) { return response.json() as Promise<{ Game?: { ArenaStatus?: string } }>; });
    if (!request) return;
    request.then(function(body) {
      var status = body && body.Game && body.Game.ArenaStatus;
      if (!status || ['waiting', 'running', 'finished', 'failed'].indexOf(status) === -1) throw new Error('invalid status');
      failures = 0;
      if (active === 'waiting' && status !== 'waiting') { window.location.reload(); return; }
      if (active === 'running' && (status === 'finished' || status === 'failed')) {
        phase = 'terminal';
        schedule(0);
        return;
      }
      schedule(delay(true));
    }).catch(function(error) {
      if (stopped || error.name === 'AbortError') return;
      retry();
    });
  }
  var manual = document.getElementById('game-manual-refresh');
  if (manual) manual.hidden = true;
  document.addEventListener('visibilitychange', function() {
    if (!document.hidden && !timer && !inFlight && !stopped) schedule(0);
  });
  window.addEventListener('pagehide', function() { stop(false); });
  window.addEventListener('pageshow', function(event) {
    if (!event.persisted) return;
    stopped = false;
    timer = null;
    phase = 'status';
    var fallback = document.getElementById('game-manual-refresh');
    if (fallback) fallback.hidden = true;
    schedule(0);
  });
  schedule(2000);
})();
