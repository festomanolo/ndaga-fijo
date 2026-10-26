# Deploying to Hostinger

The site is plain static files (no build step). It is designed to live at
**https://festomanolo.com/ndaga-fijo/**.

1. In hPanel open **Files → File Manager → public_html**.
2. Create a folder named `ndaga-fijo` (if it doesn't exist).
3. Upload `ndaga-fijo-website.zip` into `public_html/ndaga-fijo/` and **Extract** it there.
   `index.html` must end up directly inside `public_html/ndaga-fijo/` (not in a sub-folder).
4. Make sure hidden files are shown, and that `.htaccess` was extracted (compression, caching, security headers).
5. **robots.txt:** search engines only read it at the domain root. Add this line to
   `public_html/robots.txt` (create the file if it doesn't exist):

       Sitemap: https://festomanolo.com/ndaga-fijo/sitemap.xml

6. Check https://festomanolo.com/ndaga-fijo/ loads, then submit the sitemap in Google Search Console and Bing Webmaster Tools.

Before announcing: confirm the GitHub release link (`/releases/latest`) has the DMG/ZIP attached and that
`pip install ndaga-fijo` works, because the site links to both.
