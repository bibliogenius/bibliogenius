-- Add price column to books table for bookseller profile
-- The price is optional and represents the default price for all copies of this book

ALTER TABLE books ADD COLUMN price REAL;
