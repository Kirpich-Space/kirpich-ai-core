import bpy
import os

# Clear existing objects to have a clean scene
if bpy.context.object and bpy.context.object.mode != 'OBJECT':
    bpy.ops.object.mode_set(mode='OBJECT')
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete()

# Create a cube
bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 0))
cube = bpy.context.active_object

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.use_nodes = True
nodes = mat.node_tree.nodes
for node in nodes:
    if node.type == 'BSDF_PRINCIPLED':
        node.inputs['Base Color'].default_value = (1, 0, 0, 1)

# Assign material to cube
cube.data.materials.append(mat)

# Add light so the color is visible
bpy.ops.object.light_add(type='SUN', location=(5, 5, 5))

# Add camera and set it as active
bpy.ops.object.camera_add(location=(4, -4, 3), rotation=(1.1, 0, 0.78))
cam = bpy.context.active_object
bpy.context.scene.camera = cam

# Set render resolution
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240

# Define output path
output_path = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_ru_r1/task2.png"

# Ensure the directory exists
directory = os.path.dirname(output_path)
if not os.path.exists(directory):
    os.makedirs(directory)

# Configure render settings
scene.render.filepath = output_path
scene.render.image_settings.file_format = 'PNG'

# Render the scene
bpy.ops.render.render(write_still=True)