// W3C select-only combobox: open it, pick an option.
import { APG } from '../lib.mjs';
export default {
  name: 'apg-combobox',
  auth: false,
  steps: [
    { do: 'open', url: `${APG}/combobox/examples/combobox-select-only.html` },
    { do: 'click', role: 'combobox', name: 'Favorite Fruit' },
    { do: 'click', role: 'option', name: 'Huckleberry' },
    { do: 'look', interactive: true },
  ],
};
