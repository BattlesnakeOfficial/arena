// Country select combobox (DEV-1897).
//
// Enhances the native `select[data-country-select]` (a plain 249-option
// country list that works without JS) with a searchable combobox. The
// native select stays in the DOM as the real form control — the combobox
// only mirrors committed choices into it, so submitting unmatched typed
// text leaves the select's last committed value in place.
(() => {
  const maybeSelect = document.querySelector<HTMLSelectElement>(
    'select[data-country-select]',
  );
  if (!maybeSelect) return;
  const select = maybeSelect;

  interface Item {
    value: string;
    text: string;
  }

  const items: Item[] = Array.from(select.options).map((option) => ({
    value: option.value,
    text: option.textContent?.trim() ?? '',
  }));
  // The markup guarantees the first option is "None" (value '').
  const none = items[0];

  const inputId = select.id ? `${select.id}-input` : 'country-select-input';
  const listboxId = select.id ? `${select.id}-listbox` : 'country-select-listbox';

  select.style.display = 'none';

  const wrapper = document.createElement('div');
  wrapper.className = 'country-select-wrap';

  const input = document.createElement('input');
  input.type = 'text';
  input.id = inputId;
  input.placeholder = 'Search countries\u2026';
  input.autocomplete = 'off';
  input.setAttribute('role', 'combobox');
  input.setAttribute('aria-expanded', 'false');
  input.setAttribute('aria-autocomplete', 'list');
  input.setAttribute('aria-controls', listboxId);

  const listbox = document.createElement('ul');
  listbox.id = listboxId;
  listbox.className = 'country-select-list';
  listbox.setAttribute('role', 'listbox');
  listbox.hidden = true;

  wrapper.append(input, listbox);
  select.insertAdjacentElement('afterend', wrapper);

  // Repoint the field's label at the visible combobox so it keeps its
  // accessible name and click-to-focus. Without JS the label still
  // targets the select.
  const label = document.querySelector<HTMLLabelElement>(
    `label[for="${select.id}"]`,
  );
  if (label) label.htmlFor = inputId;

  const initial = items.find((item) => item.value === select.value);
  input.value = initial && initial.value ? initial.text : '';

  let open = false;
  let activeIndex = -1;
  let filtered: Item[] = [];

  // Stable, unique option id per item (values are unique: '' plus the
  // 249 ISO codes). Stable across re-filtering, so aria-activedescendant
  // keeps pointing at the same country as the highlight moves.
  function optionId(item: Item): string {
    return `${listboxId}-opt-${item.value ? item.value.toLowerCase() : 'none'}`;
  }

  function matches(item: Item, query: string): boolean {
    if (!query) return true;
    const q = query.toLowerCase();
    return (
      item.text.toLowerCase().includes(q) || item.value.toLowerCase().includes(q)
    );
  }

  function setActive(index: number): void {
    activeIndex = index;
    listbox.querySelectorAll('li').forEach((option, i) => {
      const isActive = i === index;
      option.classList.toggle('active', isActive);
      if (isActive) option.setAttribute('aria-selected', 'true');
      else option.removeAttribute('aria-selected');
    });
    // Screen readers follow the highlight through this attribute; clear
    // it when nothing is active (see closeList).
    const item = filtered[index];
    if (item) input.setAttribute('aria-activedescendant', optionId(item));
    else input.removeAttribute('aria-activedescendant');
    const active = listbox.children[index];
    if (active instanceof HTMLElement) active.scrollIntoView({ block: 'nearest' });
  }

  function render(query: string): void {
    filtered = [none, ...items.filter((item) => item.value && matches(item, query))];
    listbox.textContent = '';
    for (const item of filtered) {
      const li = document.createElement('li');
      li.id = optionId(item);
      li.setAttribute('role', 'option');
      li.textContent = item.value ? `${item.text} (${item.value})` : item.text;
      // pointerdown (not click) commits before the browser moves focus,
      // so the listbox never blinks shut mid-tap.
      li.addEventListener('pointerdown', (event) => {
        event.preventDefault();
        commit(item);
      });
      listbox.append(li);
    }
    // Default active: the first country match when searching (so Enter
    // picks it), "None" when the query is empty.
    setActive(query && filtered.length > 1 ? 1 : 0);
  }

  function openList(): void {
    render(input.value);
    open = true;
    input.setAttribute('aria-expanded', 'true');
    listbox.hidden = false;
  }

  function closeList(): void {
    open = false;
    input.setAttribute('aria-expanded', 'false');
    input.removeAttribute('aria-activedescendant');
    listbox.hidden = true;
  }

  function commit(item: Item): void {
    select.value = item.value;
    input.value = item.value ? item.text : '';
    closeList();
  }

  input.addEventListener('focus', openList);

  input.addEventListener('input', () => {
    if (input.value === '') select.value = '';
    if (open) render(input.value);
    else openList();
  });

  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!open) {
        openList();
        return;
      }
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      const next = activeIndex + delta;
      if (next >= 0 && next < filtered.length) setActive(next);
      return;
    }
    if (event.key === 'Enter') {
      // Enter with the listbox open commits the active option instead of
      // submitting the form (a second Enter, list closed, submits).
      if (open) {
        event.preventDefault();
        if (activeIndex >= 0 && activeIndex < filtered.length) {
          commit(filtered[activeIndex]);
        }
      }
      return;
    }
    if (event.key === 'Escape') {
      closeList();
    }
  });

  wrapper.addEventListener('focusout', (event) => {
    if (wrapper.contains(event.relatedTarget as Node | null)) return;
    closeList();
    // Keep the visible text in sync with the value the form will submit.
    // Text that exactly matches an option (display name or code,
    // case-insensitive) commits; anything else restores the currently
    // selected country's display name (empty when unset), so unmatched
    // typing can never masquerade as a choice.
    const typed = input.value.trim();
    const match = items.find(
      (item) =>
        item.text.toLowerCase() === typed.toLowerCase() ||
        item.value.toLowerCase() === typed.toLowerCase(),
    );
    if (match) {
      commit(match);
    } else {
      const selected = items.find((item) => item.value === select.value);
      input.value = selected && selected.value ? selected.text : '';
    }
  });
})();
