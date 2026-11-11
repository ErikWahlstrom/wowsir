// W3C date picker in a modal dialog: type a date, open the dialog, move a day with the keyboard, confirm.
import { APG } from '../lib.mjs';
export default {
  name: 'apg-datepicker',
  auth: false,
  steps: [
    { do: 'open', url: `${APG}/dialog-modal/examples/datepicker-dialog.html` },
    { do: 'fill', role: 'textbox', name: 'Date', value: '11/10/2026' },
    { do: 'click', role: 'button', name: 'Choose Date' },
    { do: 'expectText', text: 'November 2026' },
    { do: 'press', key: 'ArrowRight' },
    { do: 'press', key: 'Enter' },
    { do: 'look', interactive: true },
  ],
};
