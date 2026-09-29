-- When the user hid the "claim your play account" prompt on /me. NULL means
-- the prompt still shows, unless they've already claimed a play account.
ALTER TABLE users ADD COLUMN claim_prompt_dismissed_at TIMESTAMPTZ;
