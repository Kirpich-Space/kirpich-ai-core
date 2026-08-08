

import bpy

# Remove all objects from the default scene (Cube, Camera, Light, etc.)
bpy.ops.object.select_all(action='SELECT')
for obj in bpy.context.selected_objects:
    bpy.context.collection.objects.remove(obj)

# Add a cube at origin
cube = bpy.ops.mesh.primitive_cube_add(location=(0.0, 0.0, 0.0)).object

# Create red material if not exists
mat = bpy.data.materials.get('Red')
if mat is None:
    mat = bpy.data.materials.new(name='Red')
mat.use_nodes = True
node_tree = mat.node_tree
output_node = node_tree.nodes['Principled BSDF']
for i in range(3):
    output_node.inputs[i].default_value[0] = 1.0  # R, G, B
    output_node.inputs[i].default_value[2] = 1.0   # Alpha

# Assign material to cube
cube.materials.clear()
cube.materials.append(mat)

# Delete the default camera (it was removed already) and add a new camera
camera = bpy.ops.object.camera_add(location=(2, -10, 5)).object

# Set render resolution
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.7_en_r3/task2.png"

# Render the scene (background rendering)
bpy.ops.render.render(use_viewport=False, view_only=False)