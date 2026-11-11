// Single-page app: add todos, complete one, switch filters (hash routes).
// Measures stale views: does each click's response show the page it led to?
export default {
  name: 'todomvc-filters',
  auth: false,
  steps: [
    { do: 'open', url: 'https://demo.playwright.dev/todomvc/' },
    { do: 'fill', role: 'textbox', name: 'What needs to be done?', value: 'Buy milk' },
    { do: 'press', key: 'Enter' },
    { do: 'fill', role: 'textbox', name: 'What needs to be done?', value: 'Walk the dog' },
    { do: 'press', key: 'Enter' },
    { do: 'expectText', text: '2 items left' },
    { do: 'click', role: 'checkbox', name: 'Toggle Todo' },
    { do: 'expectText', text: '1 item left' },
    { do: 'click', role: 'link', name: 'Active' },
    { do: 'expectUrl', contains: '#/active' },
    { do: 'click', role: 'link', name: 'Completed' },
    { do: 'expectUrl', contains: '#/completed' },
    { do: 'click', role: 'link', name: 'Active' },
    { do: 'expectUrl', contains: '#/active' },
  ],
};
