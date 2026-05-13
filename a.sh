# 1. First, remove the nested .git folder (this is critical)
rm -rf settl/.git

# 2. Move all files from nested folder to main (including hidden files)
mv settl/* . 2>/dev/null
mv settl/.[!.]* . 2>/dev/null  # Moves hidden files like .env

# 3. Remove the now-empty nested directory
rmdir settl

# 4. Stage all changes
git add .

# 5. Check what's staged
git status