// Form login and logout on a public practice site (its credentials are printed on the page).
import { INTERNET } from '../lib.mjs';
export default {
  name: 'internet-login',
  auth: false,
  // the site's analytics beacon fails to resolve; it is not what these scenarios test
  noise: /optimizely|ERR_NAME_NOT_RESOLVED/,
  steps: [
    { do: 'open', url: `${INTERNET}/login` },
    { do: 'fill', role: 'textbox', name: 'Username', value: 'tomsmith' },
    { do: 'fill', role: 'textbox', name: 'Password', value: 'SuperSecretPassword!' },
    { do: 'click', role: 'button', name: 'Login' },
    { do: 'expectUrl', contains: '/secure' },
    { do: 'expectText', text: 'You logged into a secure area!' },
    { do: 'click', role: 'link', name: 'Logout' },
    { do: 'expectUrl', contains: '/login' },
    { do: 'expectText', text: 'You logged out of the secure area!' },
  ],
};
