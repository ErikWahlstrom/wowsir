// W3C tabs with automatic activation: click a tab, then move with arrow keys.
import { APG } from '../lib.mjs';
export default {
  name: 'apg-tabs',
  auth: false,
  steps: [
    { do: 'open', url: `${APG}/tabs/examples/tabs-automatic.html` },
    { do: 'click', role: 'tab', name: 'Carl Andersen' },
    { do: 'expectText', text: 'Carl Joachim Andersen' },
    { do: 'press', key: 'ArrowRight' },
    { do: 'expectText', text: 'Ida Henriette da Fonseca' },
  ],
};
