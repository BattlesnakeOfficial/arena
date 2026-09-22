-- Archive any retained failures before rolling back: this removes their history.
DROP TABLE IF EXISTS dead_letter_jobs;
