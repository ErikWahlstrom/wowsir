// Content rendered after a 5 s simulated load: the click returns long before it appears.
import { INTERNET } from '../lib.mjs';
export default {
  name: 'internet-dynamic-loading',
  auth: false,
  // the site's analytics beacon fails to resolve; it is not what these scenarios test
  noise: /optimizely|ERR_NAME_NOT_RESOLVED/,
  steps: [
    { do: 'open', url: `${INTERNET}/dynamic_loading/2` },
    { do: 'click', role: 'button', name: 'Start' },
    { do: 'expectText', text: 'Hello World!' },
  ],
};
