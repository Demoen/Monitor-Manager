from PIL import Image
import os

def create_icon():
    input_file = 'icon.png'
    output_file = 'icon.ico'
    
    if not os.path.exists(input_file):
        print(f"Error: {input_file} not found!")
        return

    try:
        img = Image.open(input_file)
        
        # Convert to RGBA if not already
        if img.mode != 'RGBA':
            img = img.convert('RGBA')
            
        # Crop to square if needed
        width, height = img.size
        if width != height:
            new_size = min(width, height)
            left = (width - new_size) / 2
            top = (height - new_size) / 2
            right = (width + new_size) / 2
            bottom = (height + new_size) / 2
            img = img.crop((left, top, right, bottom))
            print(f"Cropped image to {new_size}x{new_size}")
            
        # Save as ICO containing multiple sizes for best Windows compatibility
        img.save(output_file, format='ICO', sizes=[(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (16, 16)])
        print(f"{output_file} created successfully from {input_file}!")
        
    except Exception as e:
        print(f"Error converting icon: {e}")

if __name__ == "__main__":
    create_icon()
