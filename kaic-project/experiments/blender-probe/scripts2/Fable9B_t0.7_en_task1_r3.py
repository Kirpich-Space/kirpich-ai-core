

import bpy
from pathlib import Path

# Remove any pre-existing mesh objects (keep lights, cameras)
for obj in bpy.context.scene.collection.objects:
    if hasattr(obj, 'data') and obj.type == 'MESH':
        bpy.data.objects.remove(obj, do_unlink=True)

# Create a cube at the world origin
bpy.ops.mesh.primitive_cube_add(size=1)

# Ensure output directory exists
output_dir = Path(r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.7_en_r3")
output_dir.mkdir(parents=True, exist_ok=True)

output_path = str(output_dir / "task1.blend")

# Save the scene
bpy.ops.wm.save_as_mainfile(filepath=output_path, overwrite=True)