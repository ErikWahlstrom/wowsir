// Pages that fail: an uncaught exception on load, then a 500 response.
import { INTERNET } from '../lib.mjs';
export default {
  name: 'internet-errors',
  auth: false,
  // the site's analytics beacon fails to resolve; it is not what these scenarios test
  noise: /optimizely|ERR_NAME_NOT_RESOLVED/,
  steps: [
    { do: 'open', url: `${INTERNET}/javascript_error` },
    { do: 'open', url: `${INTERNET}/status_codes` },
    { do: 'click', role: 'link', name: '500' },
    { do: 'expectUrl', contains: '/status_codes/500' },
    { do: 'errors' },
  ],
};
